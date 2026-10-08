#!/usr/bin/env python3
"""Fail closed on SDK registry archive/index drift; exit 3 only when absent."""
from __future__ import annotations
import argparse
import hashlib
import json
from pathlib import Path
import time
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import urlopen


def read(url: str) -> bytes | None:
    try:
        with urlopen(url, timeout=10) as response:
            return response.read()
    except HTTPError as error:
        if error.code == 404:
            return None
        raise


def index_path(name: str) -> str:
    name = name.lower()
    if len(name) <= 2:
        return f"{len(name)}/{name}"
    if len(name) == 3:
        return f"3/{name[0]}/{name}"
    return f"{name[:2]}/{name[2:4]}/{name}"


def verify(registry: str, name: str, version: str, archive: Path,
           wait: bool = False, attempts: int = 30) -> bool:
    expected = hashlib.sha256(archive.read_bytes()).hexdigest()
    identity_seen = False
    for attempt in range(attempts):
        payload = read(f"{registry}/api/v1/crates/{quote(name)}/{quote(version)}/download")
        index = read(f"{registry}/{index_path(name)}")
        entries = [] if index is None else [json.loads(line) for line in index.splitlines() if line.strip()]
        matches = [entry for entry in entries if entry.get("vers") == version]
        if len(matches) > 1:
            raise ValueError(f"duplicate registry index version: {name} {version}")
        if payload is not None:
            identity_seen = True
            if hashlib.sha256(payload).hexdigest() != expected:
                raise ValueError(f"immutable Cargo archive differs: {name} {version}")
        if matches and (matches[0].get("name") != name or matches[0].get("cksum") != expected or matches[0].get("yanked")):
            raise ValueError(f"immutable Cargo index differs or is yanked: {name} {version}")
        identity_seen = identity_seen or bool(matches)
        if payload is not None and matches:
            return True
        if not wait and not identity_seen and not matches:
            return False
        # An archive without an index (or the reverse) is an interrupted
        # visibility transition, never permission to re-publish immutable bytes.
        if attempt + 1 < attempts:
            time.sleep(2)
    raise TimeoutError(f"timed out verifying Cargo archive/index: {name} {version}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("registry")
    parser.add_argument("name")
    parser.add_argument("version")
    parser.add_argument("archive", type=Path)
    parser.add_argument("--wait", action="store_true")
    args = parser.parse_args()
    raise SystemExit(0 if verify(args.registry.rstrip("/"), args.name, args.version,
                                 args.archive, args.wait) else 3)


if __name__ == "__main__":
    main()
