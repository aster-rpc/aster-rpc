#!/usr/bin/env bash
# Download all Aster binary/source assets for a Forgejo release.
# Usage: scripts/download-release.sh [latest|0.3.0|v0.3.0] [output-directory]
set -euo pipefail

requested="${1:-latest}"
output="${2:-dist-release}"
forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
api="$forge/api/v1/repos/Aster/aster-rpc-internal"
token="${FORGEJO_TOKEN:-${ASTER_REPO_TOKEN:-}}"
auth=()
if [[ -n "$token" ]]; then
  auth=(-H "Authorization: token $token")
fi

if [[ "$requested" == latest ]]; then
  endpoint="$api/releases/latest"
else
  tag="${requested#v}"
  endpoint="$api/releases/tags/v$tag"
fi
release_json="$(curl --fail-with-body --silent --show-error "${auth[@]}" "$endpoint")"
mkdir -p "$output"

ASTER_RELEASE_JSON="$release_json" python3 - "$output" "$token" <<'PY'
from pathlib import Path
import json
import os
import subprocess
import sys

output = Path(sys.argv[1])
token = sys.argv[2]
release = json.loads(os.environ["ASTER_RELEASE_JSON"])
assets = release.get("assets", [])
if not assets:
    raise SystemExit(f"release {release.get('tag_name')} has no assets")
for asset in assets:
    target = output / asset["name"]
    command = ["curl", "--fail-with-body", "--location", "--silent", "--show-error"]
    if token:
        command += ["-H", f"Authorization: token {token}"]
    command += ["--output", str(target), asset["browser_download_url"]]
    subprocess.run(command, check=True)
    print(target)
PY
