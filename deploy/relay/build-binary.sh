#!/usr/bin/env bash
# Build the iroh-relay binary from the iroh fork commit that Aster pins.
#
# Usage: build-binary.sh <rust-target-triple> <output-dir> [iroh-rev]
#
# The revision defaults to `[forks.iroh] commit` in iroh-fork-manifest.toml, so
# the relay always runs the same transport code as the Aster release it ships
# with. The Dockerfile passes the revision explicitly (it has no checkout).
# The binary lands at <output-dir>/iroh-relay; the exact source revision and
# Cargo.lock hash land next to it in build-info.json.
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s relay-build %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "error: $*"; exit "${2:-1}"; }

[[ $# -ge 2 && $# -le 3 ]] || die "usage: build-binary.sh <target> <output-dir> [iroh-rev]" 64
target="$1"
out="$2"
rev="${3:-}"
repo="${IROH_FORK_REPO:-https://forge.emrul.dev/Aster/iroh.git}"

[[ "$target" =~ ^(x86_64|aarch64)-unknown-linux-gnu$ ]] || die "unsupported target: $target" 64
if [[ -z "$rev" ]]; then
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  rev="$(python3 - "$here/../../iroh-fork-manifest.toml" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    print(tomllib.load(f)["forks"]["iroh"]["commit"])
PY
)"
fi
[[ "$rev" =~ ^[0-9a-f]{40}$ ]] || die "iroh revision must be a full commit hash: $rev" 64

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

log "fetching $repo at $rev"
git init -q "$work/iroh"
git -C "$work/iroh" fetch -q --depth 1 "$repo" "$rev"
git -C "$work/iroh" -c advice.detachedHead=false checkout -q FETCH_HEAD
[[ "$(git -C "$work/iroh" rev-parse HEAD)" == "$rev" ]] || die "fetched revision differs from $rev"

# The fork's .cargo/config.toml selects clang+lld for x86_64 and a cross gcc
# for aarch64. Use the system C compiler for a native build, and the GNU cross
# toolchain only when cross compiling. RUSTFLAGS replaces the fork's lld flags.
host="$(rustc -vV | sed -n 's/^host: //p')"
upper="$(tr 'a-z-' 'A-Z_' <<<"$target")"
if [[ "$target" == "$host" ]]; then
  export "CARGO_TARGET_${upper}_LINKER=cc"
else
  export "CARGO_TARGET_${upper}_LINKER=${target%%-*}-linux-gnu-gcc"
fi
export RUSTFLAGS=""
export CARGO_PROFILE_RELEASE_STRIP=symbols

log "building iroh-relay for $target"
(cd "$work/iroh" && cargo build --locked --release -p iroh-relay --features server \
  --bin iroh-relay --target "$target")

mkdir -p "$out"
target_dir="${CARGO_TARGET_DIR:-$work/iroh/target}"
install -m 0755 "$target_dir/$target/release/iroh-relay" "$out/iroh-relay"
# The runtime image is distroless cc-debian13 (glibc 2.41). Refuse a binary
# that needs newer glibc symbols than the runtime provides.
max_glibc="$(readelf --dyn-syms -W "$out/iroh-relay" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)"
[[ -n "$max_glibc" ]] || die "could not read glibc symbol versions"
if [[ "$(printf '%s\n' "$max_glibc" GLIBC_2.41 | sort -V | tail -1)" != GLIBC_2.41 ]]; then
  die "iroh-relay needs $max_glibc, newer than the runtime's GLIBC_2.41"
fi
log "glibc requirement: $max_glibc"
lock_sha="$(sha256sum "$work/iroh/Cargo.lock" | cut -d' ' -f1)"
bin_sha="$(sha256sum "$out/iroh-relay" | cut -d' ' -f1)"
cp "$work/iroh/Cargo.lock" "$out/Cargo.lock"
printf '{"repo":"%s","rev":"%s","target":"%s","cargo_lock_sha256":"%s","binary_sha256":"%s","rustc":"%s"}\n' \
  "$repo" "$rev" "$target" "$lock_sha" "$bin_sha" "$(rustc --version)" > "$out/build-info.json"
log "built $out/iroh-relay sha256=$bin_sha"
