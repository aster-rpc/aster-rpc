#!/usr/bin/env bash
# preflight.sh -- run CI's fast checks (scripts/ci/checks.sh) on linbox, in the
# pinned CI image, before pushing to main. A main push costs a ~34-minute gate
# run (suspended for now: docs/_internal/cicd.md, "Push CI suspended"); this catches lint, feature-graph, advisory and test-collection failures
# in minutes instead.
#
#   scripts/ci/preflight.sh                  checks.sh preflight
#   scripts/ci/preflight.sh fmt deny         just those checks
#   scripts/ci/preflight.sh --purge          remove the cargo cache and image
#
# Ships the index plus working-tree edits to tracked files (stage a new file
# for it to go; untracked files never do) and .git (the build version counts
# commits). Everything a run writes lives in one directory under
# /mnt/data/aster-preflight on linbox, deleted when the run ends, however it
# ends. Kept between runs: the CI image
# and a crate-download cache (/mnt/data/aster-preflight/cache).
set -euo pipefail

HOST="${ASTER_PREFLIGHT_HOST:-emrul@192.168.1.140}"
ROOT="${ASTER_PREFLIGHT_ROOT:-/mnt/data/aster-preflight}"
JOBS="${ASTER_PREFLIGHT_JOBS:-12}"
# The `aster-forge-linux-x64` runner image: keep in step with
# portal_infra files/forgejo-runner/runner-config.yml.
IMAGE="${ASTER_CI_IMAGE:-forge.emrul.dev/aster/aster-ci@sha256:fc86c6bd633b4b7fc31b2f0b27a23a9ff88e4c0bc474b340d68cb26ca00af799}"

cd "$(dirname "$0")/../.."

if [[ "${1:-}" == "--purge" ]]; then
  ssh "$HOST" "docker run --rm -v '$ROOT':/r '$IMAGE' rm -rf /r/cache; rmdir '$ROOT' 2>/dev/null; docker image rm '$IMAGE' || true"
  exit 0
fi

checks=("$@")
[[ ${#checks[@]} -gt 0 ]] || checks=(preflight)
run="run-$(date +%Y%m%d-%H%M%S)-$$"

echo "preflight: shipping $(git rev-parse --short HEAD)$(git diff --quiet HEAD || echo ' + uncommitted edits') to $HOST:$ROOT/$run"
pack() { COPYFILE_DISABLE=1 tar --no-xattrs --no-mac-metadata --null --no-recursion -T - -czf -; }
git ls-files -z --cached | while IFS= read -r -d '' f; do
  [[ -e "$f" || -L "$f" ]] && printf '%s\0' "$f"
done | pack | ssh "$HOST" "mkdir -p '$ROOT/$run/src' && tar -xzf - -C '$ROOT/$run/src'"
# The shared git directory, so this works from a worktree too; HEAD is then
# pinned to this checkout's commit and the index rebuilt from it.
head=$(git rev-parse HEAD)
(cd "$(git rev-parse --path-format=absolute --git-common-dir)" &&
  find . ! -type s ! -path './worktrees/*' ! -name index -print0 | pack) |
  ssh "$HOST" "mkdir -p '$ROOT/$run/src/.git' && tar -xzf - -C '$ROOT/$run/src/.git' &&
    echo $head > '$ROOT/$run/src/.git/HEAD' && git -C '$ROOT/$run/src' reset -q"

start=$SECONDS
# The remote shell owns cleanup: its EXIT trap fires on success, failure, and
# the SIGHUP a dropped ssh session delivers.
ssh "$HOST" bash -s -- "$ROOT" "$run" "$IMAGE" "$JOBS" "${checks[@]}" <<'REMOTE'
set -euo pipefail
root=$1 run=$2 image=$3 jobs=$4; shift 4
cleanup() {
  docker rm -f "aster-preflight-$run" >/dev/null 2>&1 || true
  # Files are root-owned from the container, so remove them from one.
  docker run --rm -v "$root":/r "$image" rm -rf "/r/$run" || true
}
trap cleanup EXIT
trap 'exit 130' HUP INT TERM

# Leftovers from runs killed harder than a trap can catch.
find "$root" -maxdepth 1 -name 'run-*' -mmin +360 -printf '%f\n' 2>/dev/null |
  while read -r old; do docker run --rm -v "$root":/r "$image" rm -rf "/r/$old"; done

docker image inspect "$image" >/dev/null 2>&1 || docker pull -q "$image"
docker run --rm --name "aster-preflight-$run" \
  -v "$root/$run":/w -v "$root/cache":/cache/cargo -w /w/src \
  -e RUSTC_WRAPPER= -e CARGO_HOME=/cache/cargo -e CARGO_TARGET_DIR=/w/target \
  -e CARGO_BUILD_JOBS="$jobs" -e CARGO_TERM_COLOR=always \
  -e CARGO_PROFILE_DEV_DEBUG=line-tables-only \
  -e UV_CACHE_DIR=/w/uv -e ASTER_CHECKS_VENV=/w/venv \
  "$image" bash -c '
    set -euo pipefail
    git config --global --add safe.directory "*"
    scripts/release/prepare-build-tree.sh "$(scripts/release/version.sh)" >/dev/null
    scripts/ci/checks.sh "$@"
  ' preflight "$@"
REMOTE
echo "preflight: passed in $(( (SECONDS - start) / 60 ))m$(( (SECONDS - start) % 60 ))s"
