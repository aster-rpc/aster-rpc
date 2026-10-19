#!/usr/bin/env python3
"""Stamp every binding CHANGELOG's `## Unreleased` heading with a release.

    stamp_changelogs.py <version> [--date YYYY-MM-DD] [--root DIR]

`## Unreleased` becomes `## <version> - <date>` and a fresh, empty
`## Unreleased` is put above it for the next cycle. Idempotent: a file with no
`## Unreleased` heading, or whose Unreleased section is empty, is left alone.
Prints the files it changed.
"""

import argparse
import datetime
import re
import subprocess
import sys
from pathlib import Path

HEADING = re.compile(r"^## Unreleased[ \t]*$", re.M)
NEXT_H2 = re.compile(r"^## ", re.M)
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def stamp(text, version, date):
    """Return the stamped text, or None when there is nothing to stamp."""
    m = HEADING.search(text)
    if not m:
        return None
    nxt = NEXT_H2.search(text, m.end())
    body = text[m.end() : nxt.start() if nxt else len(text)]
    if not body.strip():
        return None
    return text[: m.start()] + f"## Unreleased\n\n## {version} - {date}" + text[m.end() :]


def changelogs(root):
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "*CHANGELOG.md"],
        check=True, capture_output=True, text=True,
    ).stdout.split()
    return [Path(root) / p for p in out]


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("version")
    ap.add_argument("--date", default=datetime.date.today().isoformat())
    ap.add_argument("--root", default=Path(__file__).resolve().parents[2])
    args = ap.parse_args(argv)
    if not VERSION.match(args.version):
        sys.exit(f"error: invalid version '{args.version}'")
    if not re.match(r"^\d{4}-\d{2}-\d{2}$", args.date):
        sys.exit(f"error: invalid date '{args.date}'")
    for path in changelogs(args.root):
        new = stamp(path.read_text(), args.version, args.date)
        if new is not None:
            path.write_text(new)
            print(path.relative_to(args.root))


if __name__ == "__main__":
    main()
