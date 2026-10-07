#!/usr/bin/env python3
"""Reject skip-duplicate conflicts and verify an immutable NuGet upload."""
import argparse
import hashlib
import json
import os
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import urlopen


def verify(file: Path, version: str, *, required: bool) -> None:
    url = os.environ["FORGEJO_SERVER_URL"].rstrip("/") + f"/api/v1/packages/Aster/nuget/aster/{version}/files"
    try:
        with urlopen(url, timeout=60) as response:
            listing = json.load(response)
    except HTTPError as error:
        if error.code != 404 or required:
            raise
        return
    matches = [p for p in listing if p["name"].lower().endswith(".nupkg")]
    expected = hashlib.sha256(file.read_bytes()).hexdigest()
    if len(matches) != 1 or matches[0]["name"].lower() != file.name.lower() or matches[0]["sha256"] != expected:
        raise ValueError("immutable NuGet version has another file inventory or checksum")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("file", type=Path)
    parser.add_argument("version")
    parser.add_argument("--required", action="store_true")
    args = parser.parse_args()
    verify(args.file, args.version, required=args.required)
