"""
Conformance tests for canonical binary encoding vectors.

Each vector id maps to a .bin file in the vectors/ directory.
The expected BLAKE3 hash for each vector is stored in hashes.json.

Note: vector ids containing '/' (e.g. "micro.string.'aster/1'") are stored
on disk with '/' replaced by '%2F' to avoid filesystem path-separator issues.
The id_to_filename() helper encodes/decodes this mapping.
"""

from pathlib import Path
import json
from aster._aster import blake3_hex
import pytest

HERE = Path(__file__).parent
VECTORS_DIR = HERE / "vectors"
HASHES_FILE = HERE / "hashes.json"


def id_to_filename(vector_id: str) -> str:
    """Convert a vector id to its .bin filename, escaping '/' as '%2F'."""
    return vector_id.replace("/", "%2F") + ".bin"


def load_hashes() -> dict[str, str]:
    return json.loads(HASHES_FILE.read_text())


HASHES = load_hashes()


def test_vector_count_matches_bin_files() -> None:
    """The number of .bin files on disk must equal the number of entries in hashes.json."""
    bin_files = list(VECTORS_DIR.glob("*.bin"))
    assert len(bin_files) == len(HASHES), (
        f"Expected {len(HASHES)} .bin files, found {len(bin_files)}. "
        f"Extra or missing: {set(f.name for f in bin_files) ^ {id_to_filename(i) for i in HASHES}}"
    )


@pytest.mark.parametrize("vector_id", sorted(HASHES.keys()))
def test_canonical_bin_hash(vector_id: str) -> None:
    """Each .bin file must hash (BLAKE3) to its expected value from hashes.json."""
    bin_path = VECTORS_DIR / id_to_filename(vector_id)
    assert bin_path.exists(), f"Binary file not found: {bin_path}"

    bin_bytes = bin_path.read_bytes()
    actual_hash = blake3_hex(bin_bytes)
    expected_hash = HASHES[vector_id]

    assert actual_hash == expected_hash, (
        f"Hash mismatch for vector '{vector_id}':\n"
        f"  expected: {expected_hash}\n"
        f"  actual:   {actual_hash}"
    )
