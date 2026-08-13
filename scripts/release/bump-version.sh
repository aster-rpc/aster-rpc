#!/usr/bin/env bash
# Bump the major/minor base. The bump commit itself derives patch 0.
#
#   scripts/release/bump-version.sh 0.4
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
new="${1:?usage: bump-version.sh <major.minor>  (for example 0.4)}"
[[ "$new" =~ ^[0-9]+\.[0-9]+$ ]] || {
  echo "error: '$new' is not <major.minor>" >&2
  exit 2
}
if [[ -n "$(git -C "$repo" status --porcelain)" ]]; then
  echo "error: commit or stash all work before bumping the version base" >&2
  exit 1
fi

old_static="$(python3 - "$repo/pyproject.toml" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
match = re.search(r'(?m)^version\s*=\s*"([^"]+)"$', text)
if not match:
    raise SystemExit("project version not found")
print(match.group(1))
PY
)"
new_static="$new.0"
count="$(git -C "$repo" rev-list --count HEAD)"

python3 - "$repo" "$old_static" "$new_static" <<'PY'
from pathlib import Path
import json
import re
import sys

repo = Path(sys.argv[1])
old, new = sys.argv[2:4]

cargo_files = [
    "core/Cargo.toml", "ffi/Cargo.toml", "aster/Cargo.toml",
    "aster-expose/Cargo.toml", "aster-macros/Cargo.toml",
    "aster-transport-salvo/Cargo.toml", "bindings/python/rust/Cargo.toml",
    "bindings/typescript/native/Cargo.toml",
]
for relative in cargo_files:
    path = repo / relative
    text = path.read_text()
    text, changed = re.subn(
        rf'(?m)^version\s*=\s*"{re.escape(old)}"$',
        f'version = "{new}"', text, count=1,
    )
    if changed != 1:
        raise SystemExit(f"expected package version {old} in {relative}")
    text = text.replace(f'version = "{old}"', f'version = "{new}"')
    text = text.replace(f'version = "={old}"', f'version = "={new}"')
    path.write_text(text)

for relative in ["pyproject.toml", "cli/pyproject.toml"]:
    path = repo / relative
    text = path.read_text().replace(f'version = "{old}"', f'version = "{new}"')
    text = text.replace(f'aster-rpc>={old}', f'aster-rpc>={new}')
    path.write_text(text)

for relative in [
    "bindings/typescript/native/package.json",
    "bindings/typescript/packages/aster/package.json",
]:
    path = repo / relative
    data = json.loads(path.read_text())
    data["version"] = new
    for group in ("dependencies", "optionalDependencies"):
        for name, value in data.get(group, {}).items():
            if name.startswith("@aster-rpc/") and value == old:
                data[group][name] = new
    path.write_text(json.dumps(data, indent=2) + "\n")

bun_lock = repo / "bindings/typescript/bun.lock"
bun_text = bun_lock.read_text()
bun_text = re.sub(
    rf'("@aster-rpc/[^"@]+":\s*)"{re.escape(old)}"',
    rf'\1"{new}"',
    bun_text,
)
bun_lock.write_text(bun_text)

for path in (repo / "bindings/java").rglob("pom.xml"):
    path.write_text(path.read_text().replace(f"<version>{old}-SNAPSHOT</version>",
                                             f"<version>{new}-SNAPSHOT</version>"))

dotnet = repo / ".github/workflows/build-dotnet.yml"
dotnet.write_text(dotnet.read_text().replace(f'BASE="{old}"', f'BASE="{new}"'))
PY

cat > "$repo/VERSION" <<EOF
# Version base for the derived Aster version: <major.minor>.<commit-count - offset>.
# Bump major/minor with scripts/release/bump-version.sh; never edit by hand.
# The offset is the commit count of the bump commit, so that commit is patch 0.
$new $((count + 1))
EOF

(cd "$repo" && cargo metadata --format-version 1 >/dev/null)
(cd "$repo" && uv lock --offline)
(cd "$repo/bindings/typescript" && bun install --lockfile-only)

git -C "$repo" add VERSION Cargo.toml Cargo.lock pyproject.toml uv.lock \
  core/Cargo.toml ffi/Cargo.toml aster/Cargo.toml aster-expose/Cargo.toml \
  aster-macros/Cargo.toml aster-transport-salvo/Cargo.toml \
  bindings/python/rust/Cargo.toml bindings/typescript/native/Cargo.toml \
  bindings/typescript/native/package.json \
  bindings/typescript/packages/aster/package.json bindings/typescript/bun.lock \
  cli/pyproject.toml \
  bindings/java .github/workflows/build-dotnet.yml
git -C "$repo" commit -m "chore(release): version base $new"

actual="$("$repo/scripts/release/version.sh")"
[[ "$actual" == "$new.0" ]] || {
  echo "error: bump commit derived $actual, expected $new.0" >&2
  exit 1
}
echo "version is now $actual"
