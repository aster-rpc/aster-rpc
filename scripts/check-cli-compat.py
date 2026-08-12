#!/usr/bin/env python3
"""Verify that aster-cli follows the current aster-rpc compatibility series.

Guards against this footgun: you bump aster-rpc to 0.4.0 (breaking change),
publish it, but leave aster-cli's dependency as `aster-rpc>=0.3.0`. An old
copy of aster-cli on PyPI will then pull the new aster-rpc on install and
crash at import time.

Rule enforced:
  cli/pyproject.toml must declare `aster-rpc>=X.Y.Z` in the same major/minor
  series as the repo-root project. Patch versions are commit counts and are
  backwards compatible within that series, so the lower patch may stay at 0.

Exits 0 on pass, 1 on fail with a fix-it hint.

Run manually: python3 scripts/check-cli-compat.py
Invoked from:
  - .githooks/pre-push
  - .github/workflows/ci.yml (lint job)
  - .forgejo/workflows/linux.yml (lint job)
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
FRAMEWORK_PYPROJECT = ROOT / "pyproject.toml"
CLI_PYPROJECT = ROOT / "cli" / "pyproject.toml"


def parse_version(s: str) -> tuple[int, ...]:
    """Parse 'X.Y.Z' (or 'X.Y.Z.devN') into a sortable tuple. Dev tail is dropped."""
    core = s.split(".dev")[0].split("+")[0]
    parts = core.split(".")
    try:
        return tuple(int(p) for p in parts)
    except ValueError:
        print(f"error: cannot parse version {s!r}", file=sys.stderr)
        sys.exit(2)


def read_framework_version() -> str:
    for line in FRAMEWORK_PYPROJECT.read_text().splitlines():
        m = re.match(r'^version\s*=\s*"([^"]+)"', line)
        if m:
            return m.group(1)
    print(f"error: no `version = \"...\"` line in {FRAMEWORK_PYPROJECT}", file=sys.stderr)
    sys.exit(2)


def read_cli_lower_bound() -> str | None:
    """Return the X.Y.Z from `aster-rpc>=X.Y.Z` in cli/pyproject.toml, or None."""
    for line in CLI_PYPROJECT.read_text().splitlines():
        m = re.search(r'"aster-rpc\s*>=\s*([^"\s,]+)"', line)
        if m:
            return m.group(1)
    return None


def main() -> int:
    framework_version = read_framework_version()
    cli_lower = read_cli_lower_bound()

    if cli_lower is None:
        print(
            "error: cli/pyproject.toml must declare `aster-rpc>=X.Y.Z` "
            "(not an unpinned bare `aster-rpc`).",
            file=sys.stderr,
        )
        print(
            f"  Fix: set the dep to \"aster-rpc>={framework_version}\".",
            file=sys.stderr,
        )
        return 1

    framework = parse_version(framework_version)
    cli = parse_version(cli_lower)
    if len(framework) < 3 or len(cli) < 3:
        print("error: framework and CLI versions must be X.Y.Z", file=sys.stderr)
        return 2

    if cli[:2] != framework[:2] or cli > framework:
        print(
            f"error: aster-cli's aster-rpc lower bound ({cli_lower}) is "
            f"incompatible with the framework series ({framework_version}).",
            file=sys.stderr,
        )
        print(
            "  Major/minor bumps define compatibility boundaries; commit-count\n"
            "  patch releases remain compatible within one series.",
            file=sys.stderr,
        )
        expected = f"{framework[0]}.{framework[1]}.0"
        print(
            f'  Fix: in cli/pyproject.toml, change "aster-rpc>={cli_lower}" '
            f'to "aster-rpc>={expected}".',
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
