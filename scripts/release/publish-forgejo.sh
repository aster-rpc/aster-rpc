#!/usr/bin/env bash
# Create/update the Forgejo release for the current version and upload dist/*.
# Intended for the trusted tag workflow; ASTER_RELEASE_TOKEN needs release write.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
dist="${1:-$repo/dist}"
: "${ASTER_RELEASE_TOKEN:?ASTER_RELEASE_TOKEN is required}"
[[ -d "$dist" ]] || { echo "error: missing artifact directory $dist" >&2; exit 1; }

version="$("$repo/scripts/release/version.sh")"
tag="v$version"
ref_name="${FORGEJO_REF_NAME:-${GITHUB_REF_NAME:-}}"
[[ -z "$ref_name" || "$ref_name" == "$tag" ]] || {
  echo "error: tag $ref_name does not match derived version $tag" >&2
  exit 1
}

python3 - "$dist" "$version" <<'PY'
from pathlib import Path
import email.parser
import sys
import tarfile
import zipfile

dist = Path(sys.argv[1])
expected = sys.argv[2]
artifacts = sorted(
    path for path in dist.iterdir()
    if path.is_file() and (path.suffix == ".whl" or path.name.endswith(".tar.gz"))
)
if not artifacts:
    raise SystemExit(f"no artifacts found in {dist}")
for path in artifacts:
    if path.suffix == ".whl":
        with zipfile.ZipFile(path) as archive:
            names = [name for name in archive.namelist() if name.endswith(".dist-info/METADATA")]
            if len(names) != 1:
                raise SystemExit(f"{path.name}: expected one METADATA file")
            metadata = email.parser.BytesParser().parsebytes(archive.read(names[0]))
    elif path.name.endswith(".tar.gz"):
        with tarfile.open(path, "r:gz") as archive:
            members = [member for member in archive.getmembers() if member.name.endswith("/PKG-INFO")]
            if len(members) != 1:
                raise SystemExit(f"{path.name}: expected one PKG-INFO file")
            source = archive.extractfile(members[0])
            if source is None:
                raise SystemExit(f"{path.name}: cannot read PKG-INFO")
            metadata = email.parser.BytesParser().parse(source)
    actual = metadata.get("Version")
    if actual != expected:
        raise SystemExit(f"{path.name}: metadata version {actual!r}, expected {expected!r}")
PY

forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
api="$forge/api/v1/repos/Aster/aster-rpc-internal"
auth=(-H "Authorization: token $ASTER_RELEASE_TOKEN")
request() {
  local method="$1" path="$2"; shift 2
  curl --fail-with-body --silent --show-error -X "$method" "${auth[@]}" "$@" "$api$path"
}

release_id="$(request GET "/releases/tags/$tag" 2>/dev/null \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])' 2>/dev/null || true)"
if [[ -z "$release_id" ]]; then
  payload="$(python3 - "$tag" "$version" "$(git -C "$repo" rev-parse HEAD)" <<'PY'
import json, sys
tag, version, commit = sys.argv[1:4]
print(json.dumps({
    "tag_name": tag,
    "name": f"Aster {version}",
    "body": f"Aster {version}, built from `{commit}`.\n\nAssets include Linux, Windows, and Apple Silicon macOS Python abi3 wheels plus the source distribution.",
    "draft": False,
    "prerelease": False,
}))
PY
)"
  release_id="$(request POST /releases -H 'Content-Type: application/json' --data "$payload" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')"
fi

python3 - "$dist" <<'PY'
from pathlib import Path
import hashlib
import sys

dist = Path(sys.argv[1])
checksum_file = dist / "SHA256SUMS"
artifacts = sorted(
    path for path in dist.iterdir()
    if path.is_file() and path != checksum_file
)
with checksum_file.open("w") as output:
    for path in artifacts:
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        output.write(f"{digest}  {path.name}\n")
PY
for artifact in "$dist"/*; do
  name="$(basename "$artifact")"
  asset_id="$(request GET "/releases/$release_id/assets" \
    | python3 -c 'import json,sys; name=sys.argv[1]; print(next((str(a["id"]) for a in json.load(sys.stdin) if a["name"] == name), ""))' "$name")"
  if [[ -n "$asset_id" ]]; then
    request DELETE "/releases/$release_id/assets/$asset_id" >/dev/null
  fi
  request POST "/releases/$release_id/assets?name=$name" \
    -F "attachment=@$artifact;type=application/octet-stream" >/dev/null
  echo "uploaded $name"
done

echo "published $forge/Aster/aster-rpc-internal/releases/tag/$tag"
