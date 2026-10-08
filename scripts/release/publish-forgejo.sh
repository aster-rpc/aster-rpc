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
commit="$(git -C "$repo" rev-parse HEAD)"
body="Aster $version, built from $commit. Assets are the verified private mirror of the publicly published packages."
exec python3 "$repo/scripts/release/forgejo-release-assets.py" \
  --body "$body" "$tag" "Aster $version" "$dist"/*
