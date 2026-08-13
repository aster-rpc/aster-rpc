#!/usr/bin/env bash
# Publish versioned Python/native artifacts as anonymously readable Forgejo
# organization packages. The private repository release remains the maintainer
# record; consumers download from this public package boundary.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
dist="${1:-$repo/dist}"
: "${ASTER_PACKAGE_TOKEN:?ASTER_PACKAGE_TOKEN is required}"
[[ -d "$dist" ]] || { echo "error: missing artifact directory $dist" >&2; exit 1; }

version="$("$repo/scripts/release/version.sh")"
tag="v$version"
ref_name="${FORGEJO_REF_NAME:-${GITHUB_REF_NAME:-}}"
[[ -z "$ref_name" || "$ref_name" == "$tag" ]] || {
  echo "error: tag $ref_name does not match derived version $tag" >&2
  exit 1
}
if ! git -C "$repo" tag --points-at HEAD | grep -Fqx -- "$tag"; then
  echo "error: refusing to publish untagged HEAD; expected $tag" >&2
  exit 1
fi

# Verify embedded artifact versions and generate a portable checksum manifest.
python3 - "$dist" "$version" <<'PY'
from pathlib import Path
import email.parser
import hashlib
import sys
import tarfile
import zipfile

dist = Path(sys.argv[1])
expected = sys.argv[2]
checksum_file = dist / "SHA256SUMS"
artifacts = sorted(
    path for path in dist.iterdir()
    if path.is_file()
    and path != checksum_file
    and (path.suffix == ".whl" or path.name.endswith(".tar.gz"))
)
if not artifacts:
    raise SystemExit(f"no wheel or source artifacts found in {dist}")
for path in artifacts:
    if path.suffix == ".whl":
        with zipfile.ZipFile(path) as archive:
            names = [
                name for name in archive.namelist()
                if name.endswith(".dist-info/METADATA")
            ]
            if len(names) != 1:
                raise SystemExit(f"{path.name}: expected one METADATA file")
            metadata = email.parser.BytesParser().parsebytes(archive.read(names[0]))
    else:
        with tarfile.open(path, "r:gz") as archive:
            members = [
                member for member in archive.getmembers()
                if member.name.endswith("/PKG-INFO")
            ]
            if len(members) != 1:
                raise SystemExit(f"{path.name}: expected one PKG-INFO file")
            source = archive.extractfile(members[0])
            if source is None:
                raise SystemExit(f"{path.name}: cannot read PKG-INFO")
            metadata = email.parser.BytesParser().parse(source)
    actual = metadata.get("Version")
    if actual != expected:
        raise SystemExit(
            f"{path.name}: metadata version {actual!r}, expected {expected!r}"
        )

with checksum_file.open("w") as output:
    for path in artifacts:
        output.write(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n")
PY

forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
package_api="$forge/api/v1/packages/Aster/generic/aster-python/$version"
upload_base="$forge/api/packages/Aster/generic/aster-python/$version"

remote_files="$(curl --fail --silent "$package_api/files" 2>/dev/null || printf '[]')"
sha256_file() {
  python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"
}
remote_sha256() {
  python3 -c '
import json
import sys

name = sys.argv[1]
files = json.load(sys.stdin)
print(next((item["sha256"] for item in files if item["name"] == name), ""))
' "$1" <<<"$remote_files"
}

for artifact in "$dist"/*.whl "$dist"/*.tar.gz "$dist/SHA256SUMS"; do
  [[ -f "$artifact" ]] || continue
  name="$(basename "$artifact")"
  local_digest="$(sha256_file "$artifact")"
  published_digest="$(remote_sha256 "$name")"
  if [[ -n "$published_digest" ]]; then
    [[ "$published_digest" == "$local_digest" ]] || {
      echo "error: immutable public artifact $name already exists with another checksum" >&2
      exit 1
    }
    echo "$name already published; skipping"
    continue
  fi
  curl --fail-with-body --silent --show-error \
    --user "agents:${ASTER_PACKAGE_TOKEN}" \
    --upload-file "$artifact" \
    "$upload_base/$name"
  echo "published $name"
done

# Re-read Forgejo's own digests so a successful response is not the only proof.
published_files="$(curl --fail --silent "$package_api/files")"
for artifact in "$dist"/*.whl "$dist"/*.tar.gz "$dist/SHA256SUMS"; do
  [[ -f "$artifact" ]] || continue
  name="$(basename "$artifact")"
  expected_digest="$(sha256_file "$artifact")"
  actual_digest="$(python3 -c '
import json
import sys

name = sys.argv[1]
print(next((item["sha256"] for item in json.load(sys.stdin) if item["name"] == name), ""))
' "$name" <<<"$published_files")"
  [[ "$actual_digest" == "$expected_digest" ]] || {
    echo "error: public artifact verification failed for $name" >&2
    exit 1
  }
done

echo "published $forge/Aster/-/packages/generic/aster-python/$version"
