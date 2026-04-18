"""Generate the golden contract_id for the cross-language Echo service.

Defines an EchoService whose on-wire types use the same @wire_type tags
the Java fixture uses, then prints the resulting ``contract_id``. The
Java side of the acid test hardcodes this hash and asserts its
ContractManifestBuilder produces the same one.

Run from the repo root via:

    uv run python scripts/cross_lang_echo_contract_id.py
"""

from __future__ import annotations

from dataclasses import dataclass

from aster import wire_type
from aster.contract.identity import contract_id_from_service
from aster.decorators import rpc, service


@wire_type("echo/EchoRequest")
@dataclass
class EchoRequest:
    message: str = ""


@wire_type("echo/EchoResponse")
@dataclass
class EchoResponse:
    reply: str = ""


@service(name="EchoService", version=1)
class EchoService:
    @rpc
    async def echo(self, req: EchoRequest) -> EchoResponse:  # noqa: D401
        return EchoResponse(reply=req.message)


def _dump_debug() -> None:
    """Print the full type-graph + ServiceContract JSON the Rust FFI sees. For diffing against Java."""
    import json

    from aster.contract.identity import (
        ServiceContract,
        build_type_graph,
        canonical_xlang_bytes,
        compute_contract_id,
        compute_type_hash,
        resolve_with_cycles,
    )

    service_info = EchoService.__aster_service_info__  # type: ignore[attr-defined]
    root_types = []
    for mi in service_info.methods.values():
        if mi.request_type is not None and isinstance(mi.request_type, type):
            root_types.append(mi.request_type)
        if mi.response_type is not None and isinstance(mi.response_type, type):
            root_types.append(mi.response_type)

    types = build_type_graph(root_types)
    type_defs = resolve_with_cycles(types)

    type_hashes = {
        fqn: compute_type_hash(canonical_xlang_bytes(td))
        for fqn, td in type_defs.items()
    }

    # Show per-TypeDef JSON.
    from aster.contract.identity import _to_json  # type: ignore[attr-defined]

    for fqn, td in type_defs.items():
        print(f"--- TypeDef {fqn} (hash={type_hashes[fqn].hex()})")
        print(_to_json(td))

    contract = ServiceContract.from_service_info(service_info, type_hashes)
    sc_json = _to_json(contract)
    print("--- ServiceContract JSON")
    print(sc_json)

    print("--- contract_id")
    print(compute_contract_id(canonical_xlang_bytes(contract)))


if __name__ == "__main__":
    import sys

    if "--debug" in sys.argv:
        _dump_debug()
    else:
        print(contract_id_from_service(EchoService))
