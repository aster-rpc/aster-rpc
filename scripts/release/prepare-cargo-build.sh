#!/usr/bin/env bash
# Stamp the exact derived version into Aster library/binding Cargo metadata.
#
# Cargo manifests must contain a literal package version, while Aster's patch
# version is derived from the commit count. Keep the committed manifests at the
# major/minor baseline (for example 0.3.0), then run this in disposable build
# trees before invoking Cargo. The corresponding local Cargo.lock entries are
# updated at the same time so subsequent --locked builds remain reproducible.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
version="${1:-$("$repo/scripts/release/version.sh")}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
  echo "error: '$version' is not a release version" >&2
  exit 2
}

python3 - "$repo" "$version" <<'PY'
from pathlib import Path
import re
import sys

repo = Path(sys.argv[1])
version = sys.argv[2]

version_lines = [
    line.split("#", 1)[0].strip()
    for line in (repo / "VERSION").read_text().splitlines()
]
version_lines = [line for line in version_lines if line]
if len(version_lines) != 1:
    raise SystemExit("expected one version-base line in VERSION")

series = version_lines[0].split()[0]
if not re.fullmatch(r"\d+\.\d+", series):
    raise SystemExit(f"invalid version series {series!r} in VERSION")
if version.rsplit(".", 1)[0] != series:
    raise SystemExit(
        f"build version {version} does not belong to VERSION series {series}"
    )
baseline = f"{series}.0"

packages = {
    "core/Cargo.toml": "aster_transport_core",
    "ffi/Cargo.toml": "aster_transport_ffi",
    "aster/Cargo.toml": "aster",
    "aster-expose/Cargo.toml": "aster-expose",
    "aster-macros/Cargo.toml": "aster-macros",
    "aster-transport-salvo/Cargo.toml": "aster-transport-salvo",
    "bindings/python/rust/Cargo.toml": "aster_rs",
    "bindings/typescript/native/Cargo.toml": "aster-transport-napi",
}

package_block_re = re.compile(
    r"(?ms)^\[package\]\s*$.*?(?=^\[|\Z)"
)
version_line_re = re.compile(r'(?m)^(\s*version\s*=\s*")([^"]+)(".*)$')

for relative, expected_name in packages.items():
    path = repo / relative
    text = path.read_text()
    block_match = package_block_re.search(text)
    if not block_match:
        raise SystemExit(f"missing [package] section in {relative}")
    block = block_match.group(0)

    name_match = re.search(r'(?m)^\s*name\s*=\s*"([^"]+)"', block)
    if not name_match or name_match.group(1) != expected_name:
        found = name_match.group(1) if name_match else None
        raise SystemExit(
            f"expected package {expected_name!r} in {relative}, found {found!r}"
        )

    version_matches = list(version_line_re.finditer(block))
    if len(version_matches) != 1:
        raise SystemExit(
            f"expected one package version in {relative}, found {len(version_matches)}"
        )
    current = version_matches[0].group(2)
    if current not in {baseline, version}:
        raise SystemExit(
            f"expected {relative} package version {baseline} (or already-stamped "
            f"{version}), found {current}"
        )
    updated_block = version_line_re.sub(rf"\g<1>{version}\g<3>", block, count=1)
    text = text[: block_match.start()] + updated_block + text[block_match.end() :]

    # Versioned path dependencies must agree with the package versions. Most
    # internal paths omit a version; these two intentionally declare one.
    if relative == "aster/Cargo.toml":
        for dependency in ("aster-expose", "aster-macros"):
            dependency_re = re.compile(
                rf'(?m)^(\s*{re.escape(dependency)}\s*=\s*\{{[^\n]*'
                rf'\bversion\s*=\s*")([^"]+)("[^\n]*\}}\s*)$'
            )
            matches = list(dependency_re.finditer(text))
            if len(matches) != 1:
                raise SystemExit(
                    f"expected one versioned {dependency} path dependency in {relative}"
                )
            current = matches[0].group(2)
            if current not in {baseline, version}:
                raise SystemExit(
                    f"expected {dependency} dependency version {baseline} "
                    f"(or {version}), found {current}"
                )
            text = dependency_re.sub(rf"\g<1>{version}\g<3>", text, count=1)

    path.write_text(text)

lock_path = repo / "Cargo.lock"
lock_text = lock_path.read_text()
package_entry_re = re.compile(r"(?ms)^\[\[package\]\]\s*$.*?(?=^\[\[package\]\]|\Z)")
seen = set()

def stamp_lock_entry(match):
    entry = match.group(0)
    name_match = re.search(r'(?m)^name = "([^"]+)"$', entry)
    if not name_match or name_match.group(1) not in packages.values():
        return entry
    name = name_match.group(1)
    if name in seen:
        raise SystemExit(f"duplicate local package {name!r} in Cargo.lock")
    seen.add(name)
    version_matches = list(version_line_re.finditer(entry))
    if len(version_matches) != 1:
        raise SystemExit(
            f"expected one Cargo.lock version for {name}, found {len(version_matches)}"
        )
    current = version_matches[0].group(2)
    if current not in {baseline, version}:
        raise SystemExit(
            f"expected Cargo.lock version {baseline} (or {version}) for {name}, "
            f"found {current}"
        )
    return version_line_re.sub(rf"\g<1>{version}\g<3>", entry, count=1)

lock_text = package_entry_re.sub(stamp_lock_entry, lock_text)
missing = set(packages.values()) - seen
if missing:
    raise SystemExit(f"missing local packages in Cargo.lock: {', '.join(sorted(missing))}")
lock_path.write_text(lock_text)
PY

echo "$version"
