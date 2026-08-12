"""The package metadata and compiled extension must identify the same build."""

import re
from pathlib import Path

import aster
import aster._aster as native


def test_build_version_is_embedded_and_exported() -> None:
    version_file = Path(__file__).parents[2] / "VERSION"
    base = next(
        line.split()[0]
        for line in version_file.read_text().splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    )
    assert re.fullmatch(rf"{re.escape(base)}\.\d+", native.VERSION)
    assert aster.VERSION == native.VERSION
    assert aster.__version__ == native.VERSION
