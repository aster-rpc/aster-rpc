"""Generate golden contract_id fixtures for cross-language conformance tests.

Two fixtures:

* ``EchoService`` -- trivial acyclic shape (one request / one response). Used by
  the Java ``CrossLanguageContractIdTest`` acid test.
* Three-node reference cycle ``Alpha -> Beta -> Gamma -> Alpha``. Exercises the
  Tarjan SCC path + bottom-up per-type hashing. The fixture is graph-level only
  (not wrapped in an ``@service``) because Python's ``@rpc`` path calls
  ``typing.get_type_hints`` on methods, and mutually-recursive forward refs trip
  recursion limits on Python 3.13. Testing the per-TypeDef hashes is enough to
  lock cycle handling across bindings; the service-level path is covered by the
  acyclic EchoService fixture.

Every binding (Python, Java, TS, Go, .NET) should mirror these two fixtures and
assert the same hashes. The expected values come from THIS script -- it is the
reference producer and the test vector store.

Run from the repo root:

    uv run python scripts/cross_lang_echo_contract_id.py
    uv run python scripts/cross_lang_echo_contract_id.py --debug
"""

from dataclasses import dataclass, field

from aster import wire_type
from aster.contract.identity import contract_id_from_service
from aster.decorators import rpc, service


# ── Fixture 1: EchoService (acyclic) ────────────────────────────────────────


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
    async def echo(self, req: EchoRequest) -> EchoResponse:
        return EchoResponse(reply=req.message)


# ── Fixture 2: Three-node reference cycle (type-graph only) ─────────────────


@wire_type("chain/Alpha")
@dataclass
class Alpha:
    name: str = ""
    betas: list["Beta"] = field(default_factory=list)


@wire_type("chain/Beta")
@dataclass
class Beta:
    name: str = ""
    gammas: list["Gamma"] = field(default_factory=list)


@wire_type("chain/Gamma")
@dataclass
class Gamma:
    name: str = ""
    alphas: list["Alpha"] = field(default_factory=list)  # back-edge to Alpha


def three_cycle_hashes() -> dict[str, str]:
    """Compute per-TypeDef hashes for the Alpha/Beta/Gamma 3-cycle.

    Each binding that mirrors the Alpha/Beta/Gamma graph (same @WireType tags,
    same field names, same ``list<TypeDef>`` containers) must produce the same
    three hex hashes. Drift indicates a divergence in Tarjan SCC ordering,
    back-edge classification, or per-TypeDef canonical byte emission.
    """
    from aster.contract.identity import (
        build_type_graph,
        canonical_xlang_bytes,
        compute_type_hash,
        resolve_with_cycles,
    )

    types = build_type_graph([Alpha])
    resolved = resolve_with_cycles(types)
    return {
        fqn: compute_type_hash(canonical_xlang_bytes(td)).hex()
        for fqn, td in resolved.items()
    }


def _dump_debug() -> None:
    from aster.contract.identity import (
        ServiceContract,
        build_type_graph,
        canonical_xlang_bytes,
        compute_contract_id,
        compute_type_hash,
        resolve_with_cycles,
    )
    from aster.contract.identity import _to_json  # type: ignore[attr-defined]

    print(f"\n{'=' * 60}\nEchoService\n{'=' * 60}")
    info = EchoService.__aster_service_info__  # type: ignore[attr-defined]
    types = build_type_graph([EchoRequest, EchoResponse])
    type_defs = resolve_with_cycles(types)
    hashes = {
        fqn: compute_type_hash(canonical_xlang_bytes(td))
        for fqn, td in type_defs.items()
    }
    for fqn, td in type_defs.items():
        print(f"--- TypeDef {fqn} (hash={hashes[fqn].hex()})")
        print(_to_json(td))
    contract = ServiceContract.from_service_info(info, hashes)
    cid = compute_contract_id(canonical_xlang_bytes(contract))
    print(f"--- ServiceContract (contract_id={cid})")
    print(_to_json(contract))

    print(f"\n{'=' * 60}\nThree-cycle (Alpha/Beta/Gamma)\n{'=' * 60}")
    cycle_types = build_type_graph([Alpha])
    for fqn, td in resolve_with_cycles(cycle_types).items():
        print(f"--- TypeDef {fqn}")
        print(_to_json(td))
    for fqn, h in three_cycle_hashes().items():
        print(f"  {fqn} -> {h}")


if __name__ == "__main__":
    import sys

    if "--debug" in sys.argv:
        _dump_debug()
    else:
        print(f"EchoService  contract_id: {contract_id_from_service(EchoService)}")
        print("3-cycle per-TypeDef hashes:")
        for fqn, h in three_cycle_hashes().items():
            print(f"  {fqn} {h}")
