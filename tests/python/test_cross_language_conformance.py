"""Cross-language conformance fixtures.

Pins the hashes that every binding must produce for the shared 3-cycle fixture
defined in ``scripts/cross_lang_echo_contract_id.py``. Any binding port is
required to mirror this graph and reproduce identical per-TypeDef hashes --
drift catches SCC-order bugs, canonical-byte emission bugs, or FQN mismatches.

The Python side of the conformance test imports the reference producer script
and calls its ``three_cycle_hashes`` helper directly. The Java side lives at
``bindings/java/aster-runtime/src/test/java/site/aster/contract/CrossLanguageCycleConformanceTest.java``
and hardcodes the same hex values. Adding TS / Go / .NET: mirror the structure
there and assert against the same values.

If the graph definition changes, regenerate:

    uv run python scripts/cross_lang_echo_contract_id.py
"""

from __future__ import annotations

import sys
from pathlib import Path


# Add repo root's `scripts/` to path so we can import the reference producer.
_REPO_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(_REPO_ROOT / "scripts"))

# Import after path setup. Script import has side-effects (module-level @wire_type dataclasses)
# which is fine -- they live in the script module, not the test module.
import cross_lang_echo_contract_id as fixture  # noqa: E402

EXPECTED_CYCLE_HASHES = {
    "chain.Alpha": "f2530f9d487afdce94b41eaa875b5aca0df1981de67185bb13796203492e2403",
    "chain.Beta":  "5a7471f00be2437f28769074a809bae2daa4e15bafba59684e4bdebd10561f94",
    "chain.Gamma": "f6c956b80cc7015cb6cbafff68ab81faac06de1b50e6b9ab0610da46dd2094e9",
}

EXPECTED_ECHO_CONTRACT_ID = (
    "12d2f2990f4dd71dfd59f5db470d186f1fcc7dbafdac0ea7fdf838ab263c0578"
)


def test_three_cycle_hashes_match_golden():
    """Per-TypeDef hashes for the Alpha/Beta/Gamma cycle stay locked.

    Bindings in other languages that mirror the fixture must produce the same
    three hex digests; the Python side here pins them so a change in the Python
    walker / SCC resolver surfaces immediately instead of silently shifting the
    cross-language reference.
    """
    actual = fixture.three_cycle_hashes()
    assert actual == EXPECTED_CYCLE_HASHES, (
        "Three-cycle hashes drifted. If intentional, update the EXPECTED map in:\n"
        "  tests/python/test_cross_language_conformance.py\n"
        "  bindings/java/.../CrossLanguageCycleConformanceTest.java\n"
        "and any other binding that pins these values.\n"
        f"Actual:   {actual}\n"
        f"Expected: {EXPECTED_CYCLE_HASHES}"
    )


def test_echo_service_contract_id_matches_golden():
    """Acyclic EchoService contract_id stays locked."""
    from aster.contract.identity import contract_id_from_service

    actual = contract_id_from_service(fixture.EchoService)
    assert actual == EXPECTED_ECHO_CONTRACT_ID, (
        "EchoService contract_id drifted. Rerun "
        "scripts/cross_lang_echo_contract_id.py and update fixtures.\n"
        f"Actual:   {actual}\n"
        f"Expected: {EXPECTED_ECHO_CONTRACT_ID}"
    )


def test_all_cycle_hashes_are_real_not_zero():
    """Sanity: the zero-placeholder bug produced non-leaf zeros. Guard against regression."""
    actual = fixture.three_cycle_hashes()
    for fqn, h in actual.items():
        assert len(h) == 64, f"{fqn} hash must be 64-char hex: {h}"
        assert h != "0" * 64, (
            f"{fqn} resolved to the zero-placeholder -- SCC ordering regressed"
        )
