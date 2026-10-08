#!/usr/bin/env bash
# Package or publish the public Rust SDK crates in dependency order.
#
# CI runs this from a disposable tree after prepare-build-tree.sh has stamped
# the derived release version. Reading the Aster registry is anonymous;
# publishing requires a write:package token on this Forgejo installation.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
mode="${1:---publish}"
case "$mode" in
  --check|--publish) ;;
  *)
    echo "usage: scripts/release/publish-cargo.sh [--check|--publish]" >&2
    exit 2
    ;;
esac

packages=(
  aster_transport_core
  aster-otel
  aster-macros
  aster-expose
  aster
  aster-transport-salvo
)

cd "$repo"

# All Cargo subprocesses share a finite deadline; no SDK test is retried.
bounded_cargo() {
  python3 - "$@" <<'PYTHON'
import os
import signal
import subprocess
import sys
process = subprocess.Popen(["cargo", *sys.argv[1:]], start_new_session=True)
try:
    status = process.wait(timeout=1800)
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGKILL)
    process.wait()
    raise SystemExit("Cargo phase exceeded 1800-second deadline")
raise SystemExit(status)
PYTHON
}

if [[ "$mode" == "--check" ]]; then
  for package in "${packages[@]}"; do
    # `--list` checks file selection without resolving the alternate-registry
    # fallback locations. The real Cargo packaging check runs immediately
    # before each upload, after that crate's dependencies are in the index.
    bounded_cargo package --locked --allow-dirty --list -p "$package" >/dev/null
  done

  # A dependency with a local path but no explicit registry is normalized to
  # crates.io. That would split the Aster package family at publication time.
  bounded_cargo metadata --locked --format-version 1 --no-deps \
    | python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
publishable = {package["name"] for package in metadata["packages"]
               if package.get("publish") == ["aster"]}
errors = []
for package in metadata["packages"]:
    if package["name"] not in publishable:
        continue
    for dependency in package["dependencies"]:
        if dependency["name"] not in publishable or not dependency.get("path"):
            continue
        registry = dependency.get("registry") or ""
        if "/api/packages/Aster/cargo/" not in registry:
            errors.append(
                f"{package['"'"'name'"'"']} -> {dependency['"'"'name'"'"']} lacks registry = \"aster\""
            )
if errors:
    print("publishable path dependencies would normalize to crates.io:", file=sys.stderr)
    print("\n".join(f"  {error}" for error in errors), file=sys.stderr)
    raise SystemExit(1)
'

  # Likewise, every forked direct dependency needs an explicit alternate
  # registry fallback. `git + version` without this field becomes crates.io in
  # the normalized package, which can compile locally while publishing the
  # wrong package identity.
  bounded_cargo metadata --locked --format-version 1 --no-deps \
    | python3 -c '
import json
import sys

metadata = json.load(sys.stdin)
errors = []
registry_bridges = {
    "h3-datagram",
    "iroh-mdns-address-lookup",
    "iroh-tickets",
    "iroh-util",
    "irpc",
    "netwatch",
    "portmapper",
}
for package in metadata["packages"]:
    if package.get("publish") != ["aster"]:
        continue
    for dependency in package["dependencies"]:
        source = dependency.get("source") or ""
        is_fork = (source.startswith("git+https://forge.emrul.dev/Aster/")
                   or source.startswith("git+https://github.com/aster-rpc/"))
        if not is_fork and dependency["name"] not in registry_bridges:
            continue
        registry = dependency.get("registry") or ""
        if "/api/packages/Aster/cargo/" not in registry:
            errors.append(
                f"{package['"'"'name'"'"']} -> {dependency['"'"'name'"'"']} lacks registry = \"aster\""
            )
if errors:
    print("fork dependencies would normalize to crates.io:", file=sys.stderr)
    print("\n".join(f"  {error}" for error in errors), file=sys.stderr)
    raise SystemExit(1)
'

  # Fory's runtime and proc macro must remain an exact validated pair. A
  # seemingly narrow `"1.3"` requirement can select a newer 1.x release in a
  # consumer lockfile even while this repository's lock still contains 1.3.0.
  bounded_cargo metadata --locked --format-version 1 --no-deps \
    | python3 -c '
import json
import sys
import tomllib

metadata = json.load(sys.stdin)
with open("iroh-fork-manifest.toml", "rb") as manifest_file:
    supported = tomllib.load(manifest_file)["upstream"]["fory"]["version"]
expected = f"={supported}"
errors = []
for package in metadata["packages"]:
    if package.get("publish") != ["aster"]:
        continue
    for dependency in package["dependencies"]:
        if dependency["name"] not in {"fory-core", "fory-derive"}:
            continue
        if dependency["req"] != expected:
            package_name = package["name"]
            dependency_name = dependency["name"]
            requirement = dependency["req"]
            errors.append(
                f"{package_name} -> {dependency_name} uses "
                f"{requirement}, expected {expected}"
            )
if errors:
    print("published Fory dependencies are not the validated exact pair:",
          file=sys.stderr)
    print("\n".join(f"  {error}" for error in errors), file=sys.stderr)
    raise SystemExit(1)
'

  # Compute the reverse registry boundary to catch second-order bridges (for
  # example portmapper -> netwatch -> noq). The manifest is the release BOM;
  # this check prevents it drifting from the resolved all-feature workspace.
  bounded_cargo metadata --locked --format-version 1 --all-features \
    | python3 -c '
import json
import re
import sys

metadata = json.load(sys.stdin)
packages = {package["id"]: package for package in metadata["packages"]}
nodes = metadata["resolve"]["nodes"]

def is_aster_fork(package):
    source = package.get("source") or ""
    return (source.startswith("git+https://forge.emrul.dev/Aster/")
            or source.startswith("git+https://github.com/aster-rpc/"))

fork_ids = {package_id for package_id, package in packages.items()
            if is_aster_fork(package)}
boundary = set(fork_ids)
bridge_ids = set()
changed = True
while changed:
    changed = False
    for node in nodes:
        package_id = node["id"]
        package = packages[package_id]
        if package_id in boundary or package.get("source") is None:
            continue
        if any(dependency["pkg"] in boundary for dependency in node["deps"]):
            boundary.add(package_id)
            bridge_ids.add(package_id)
            changed = True

actual = {packages[package_id]["name"] for package_id in bridge_ids}
with open("iroh-fork-manifest.toml", encoding="utf-8") as manifest_file:
    manifest = manifest_file.read()
fork_blocks = re.findall(
    r"(?ms)^\[forks\.[^]]+\]\s*$.*?(?=^\[|\Z)", manifest
)
expected = set()
for block in fork_blocks:
    match = re.search(
        r"(?ms)^registry_bridge_crates\s*=\s*\[(.*?)\]\s*$", block
    )
    if match:
        expected.update(re.findall(r"\"([^\"]+)\"", match.group(1)))
if actual != expected:
    missing = sorted(actual - expected)
    stale = sorted(expected - actual)
    if missing:
        print("registry bridges missing from the fork BOM: " + ", ".join(missing),
              file=sys.stderr)
    if stale:
        print("stale registry bridges in the fork BOM: " + ", ".join(stale),
              file=sys.stderr)
    raise SystemExit(1)
'
  exit 0
fi

# This is also the acceptance test for anonymous consumer access. A 401 here
# means Forgejo's owner/instance package visibility has not been opened yet.
curl --connect-timeout 10 --max-time 60 --fail-with-body --silent --show-error \
  "https://forge.emrul.dev/api/packages/Aster/cargo/config.json" >/dev/null

registry_url="https://forge.emrul.dev/api/packages/Aster/cargo"
metadata="$(bounded_cargo metadata --locked --format-version 1 --no-deps)"

package_version() {
  python3 -c '
import json
import sys

name = sys.argv[1]
matches = [package["version"] for package in json.load(sys.stdin)["packages"]
           if package["name"] == name]
if len(matches) != 1:
    raise SystemExit(f"expected one workspace package named {name}, found {len(matches)}")
print(matches[0])
' "$1" <<<"$metadata"
}

release_version="$("$repo/scripts/release/version.sh")"
release_tag="v$release_version"
if ! git -C "$repo" tag --points-at HEAD | grep -Fqx -- "$release_tag"; then
  echo "error: refusing to publish untagged HEAD; expected $release_tag" >&2
  exit 1
fi
for package in "${packages[@]}"; do
  version="$(package_version "$package")"
  [[ "$version" == "$release_version" ]] || {
    echo "error: $package is $version, expected stamped release $release_version" >&2
    exit 1
  }
done

if [[ -z "${CARGO_REGISTRIES_ASTER_TOKEN:-}" ]]; then
  : "${ASTER_CARGO_TOKEN:?set ASTER_CARGO_TOKEN (Forgejo write:package)}"
  export CARGO_REGISTRIES_ASTER_TOKEN="Bearer ${ASTER_CARGO_TOKEN}"
fi

target_directory="$(python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' <<<"$metadata")"
for package in "${packages[@]}"; do
  version="$(package_version "$package")"
  # Build the precise candidate archive before accepting an existing version.
  # Dependencies become visible in order; verification still runs for new uploads.
  bounded_cargo package --registry aster --locked --allow-dirty --no-verify -p "$package"
  archive="$target_directory/package/$package-$version.crate"
  if python3 "$repo/scripts/release/cargo-publication-identity.py" "$registry_url" "$package" "$version" "$archive"; then
    echo "${package} ${version} has identical archive and index identity; skipping"
    continue
  else
    status=$?
    [[ "$status" == 3 ]] || exit "$status"
  fi
  bounded_cargo publish --registry aster --locked --allow-dirty -p "$package" --dry-run
  bounded_cargo publish --registry aster --locked --allow-dirty -p "$package"
  python3 "$repo/scripts/release/cargo-publication-identity.py" "$registry_url" "$package" "$version" "$archive" --wait
done
