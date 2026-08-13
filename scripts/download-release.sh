#!/usr/bin/env bash
# Download public, versioned Aster Python/native artifacts from Forgejo's
# organization package registry. No repository or package token is required.
# Usage: scripts/download-release.sh [latest|0.3.0|v0.3.0|dev-20] [output-directory]
set -euo pipefail

requested="${1:-latest}"
output="${2:-dist-release}"
forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
api="$forge/api/v1"

if [[ "$requested" == latest ]]; then
  version="$(curl --fail-with-body --silent --show-error \
    "$api/packages/Aster?type=generic&q=aster-python&limit=100" \
    | python3 -c '
import json
import re
import sys

versions = []
for package in json.load(sys.stdin):
    if package.get("name") != "aster-python":
        continue
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)", package.get("version", ""))
    if match:
        versions.append((tuple(map(int, match.groups())), package["version"]))
if not versions:
    raise SystemExit("no stable public Aster artifact package is available")
print(max(versions)[1])
')"
else
  version="${requested#v}"
  [[ "$version" =~ ^([0-9]+\.[0-9]+\.[0-9]+|dev-[0-9]+)$ ]] || {
    echo "error: expected latest, a numeric Aster version, or dev-<run>" >&2
    exit 2
  }
fi

files_json="$(curl --fail-with-body --silent --show-error \
  "$api/packages/Aster/generic/aster-python/$version/files")"
mkdir -p "$output"

ASTER_FILES_JSON="$files_json" python3 - "$output" "$forge" "$version" <<'PY'
from pathlib import Path
import hashlib
import json
import os
import subprocess
import sys

output = Path(sys.argv[1])
forge = sys.argv[2]
version = sys.argv[3]
files = json.loads(os.environ["ASTER_FILES_JSON"])
if not files:
    raise SystemExit(f"Aster artifact package {version} has no files")
for item in files:
    name = item["name"]
    target = output / name
    url = f"{forge}/api/packages/Aster/generic/aster-python/{version}/{name}"
    subprocess.run(
        [
            "curl",
            "--fail-with-body",
            "--location",
            "--silent",
            "--show-error",
            "--output",
            str(target),
            url,
        ],
        check=True,
    )
    actual = hashlib.sha256(target.read_bytes()).hexdigest()
    if actual != item["sha256"]:
        raise SystemExit(f"{name}: Forgejo digest mismatch")
    print(target)

checksum_file = output / "SHA256SUMS"
if not checksum_file.is_file():
    if version.startswith("dev-"):
        print(
            f"warning: legacy development package {version} has no SHA256SUMS; "
            "Forgejo file digests were verified",
            file=sys.stderr,
        )
        raise SystemExit(0)
    raise SystemExit(f"stable Aster artifact package {version} has no SHA256SUMS")
for line in checksum_file.read_text().splitlines():
    if not line.strip():
        continue
    expected, name = line.split(None, 1)
    path = output / name.strip()
    if not path.is_file():
        raise SystemExit(f"SHA256SUMS references missing file {path.name}")
    actual = hashlib.sha256(path.read_bytes()).hexdigest()
    if actual != expected:
        raise SystemExit(f"{path.name}: checksum manifest mismatch")
PY

echo "downloaded public Aster artifacts $version"
