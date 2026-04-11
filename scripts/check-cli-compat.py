#!/usr/bin/env python3
"""Verify that aster-cli's aster-rpc lower bound is >= the current framework version.

Guards against this footgun: you bump aster-rpc to 0.2.0 (breaking change),
publish it, but leave aster-cli's dependency as `aster-rpc>=0.1.0`. An old
copy of aster-cli on PyPI will then pull the new aster-rpc on install and
crash at import time.

Rule enforced:
  cli/pyproject.toml must declare `aster-rpc>=X.Y.Z` where X.Y.Z >= the
  `version` in the repo-root pyproject.toml (the aster-rpc version).

Exits 0 on pass, 1 on fail with a fix-it hint.

Run manually: python3 scripts/check-cli-compat.py
Invoked from:
  - .githooks/pre-push
  - .github/workflows/ci.yml (lint job)
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

    if parse_version(cli_lower) < parse_version(framework_version):
        print(
            f"error: aster-cli's aster-rpc lower bound ({cli_lower}) is "
            f"older than the framework version ({framework_version}).",
            file=sys.stderr,
        )
        print(
            "  Why this matters: once aster-rpc {fw} ships to PyPI, old aster-cli\n"
            "  installs will pull the new aster-rpc and break at import time if\n"
            "  there are breaking changes.".format(fw=framework_version),
            file=sys.stderr,
        )
        print(
            f'  Fix: in cli/pyproject.toml, change "aster-rpc>={cli_lower}" '
            f'to "aster-rpc>={framework_version}".',
            file=sys.stderr,
        )
        print(
            "  If this framework bump is fully backwards compatible and you want\n"
            "  to keep the older lower bound on purpose, raise the lower bound\n"
            "  to the framework version anyway -- the CLI is tested against the\n"
            "  current framework, not against arbitrarily old ones.",
            file=sys.stderr,
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
