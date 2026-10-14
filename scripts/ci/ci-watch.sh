#!/usr/bin/env bash
# ci-watch.sh -- follow the Forgejo Actions jobs for one commit and, the
# moment a job fails, print the end of its log and the lines that explain it.
#
#   scripts/ci/ci-watch.sh               HEAD
#   scripts/ci/ci-watch.sh <sha>         that commit
#   scripts/ci/ci-watch.sh <sha> --once  one status line, no waiting
#
# Exit 0 when every job that ran succeeded, 1 on the first failure, 2 when no
# jobs appear for the commit within the wait. Reads the API with the git
# credential for forge.emrul.dev and job logs over ssh on the Forge host
# (the API cannot read logs): docs/_internal/dev-loop-runbook.md.
set -euo pipefail

FORGE="${ASTER_FORGE_URL:-https://forge.emrul.dev}"
REPO="${ASTER_FORGE_REPO:-Aster/aster-rpc-internal}"
LOG_HOST="${ASTER_FORGE_LOG_HOST:-ubuntu@51.195.190.65}"
LOG_ROOT="${ASTER_FORGE_LOG_ROOT:-/srv/portal/forgejo/forgejo-data/gitea/actions_log}"
POLL="${ASTER_CI_WATCH_POLL:-30}"
APPEAR_TIMEOUT="${ASTER_CI_WATCH_APPEAR:-600}"

cd "$(dirname "$0")/../.."
sha="$(git rev-parse "${1:-HEAD}")"
once=false
[[ "${2:-}" == "--once" ]] && once=true

token="$(printf 'protocol=https\nhost=%s\n\n' "${FORGE#https://}" | git credential fill 2>/dev/null | sed -n 's/^password=//p')"
[[ -n "$token" ]] || { echo "ci-watch: no git credential for $FORGE" >&2; exit 2; }

# One line per job for this commit: "<id> <status> <name>", newest run only.
jobs() {
  local page
  for page in 1 2 3; do
    curl -fsS -H "Authorization: token $token" \
      "$FORGE/api/v1/repos/$REPO/actions/tasks?limit=100&page=$page"
    echo
  done | python3 -c '
import json, sys
sha = sys.argv[1]
seen = {}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    for t in json.loads(line).get("workflow_runs", []):
        if t.get("head_sha") != sha:
            continue
        key = (t.get("workflow_id"), t.get("name"))
        # Tasks are newest first: keep the latest attempt of each job.
        seen.setdefault(key, t)
for (workflow, name), t in sorted(seen.items(), key=lambda kv: kv[1]["id"]):
    print(t["id"], t["status"], f"{workflow}:{name}")
' "$sha"
}

explain() {
  local id="$1" name="$2" path
  echo
  echo "=== $name (task $id) failed"
  path="$(ssh -o ConnectTimeout=15 "$LOG_HOST" "sudo find '$LOG_ROOT' -name '$id.log*' | head -1" 2>/dev/null || true)"
  if [[ -z "$path" ]]; then
    echo "(log not on disk yet; open $FORGE/$REPO/actions)"
    return
  fi
  local log
  log="$(mktemp)"
  if [[ "$path" == *.zst ]]; then
    ssh "$LOG_HOST" "sudo cat '$path'" | zstd -dc >"$log"
  else
    ssh "$LOG_HOST" "sudo cat '$path'" >"$log"
  fi
  # Strip timestamps and colour so the lines read like a terminal.
  sed -e 's/^[0-9T:.-]*Z //' -e $'s/\x1b\\[[0-9;]*m//g' "$log" >"$log.txt"
  echo "--- why (matching lines)"
  grep -n -E -- '--- FAIL|^FAIL|panicked at|error\[|^error(:|\[)|::error::|test result: FAILED|exitcode' "$log.txt" | tail -25 || true
  echo "--- last 40 lines"
  tail -40 "$log.txt"
  echo "--- full log: $log.txt"
}

start=$(date +%s)
echo "ci-watch: $(git log -1 --format='%h %s' "$sha")"
while true; do
  out="$(jobs)"
  if [[ -z "$out" ]]; then
    if $once || (( $(date +%s) - start > APPEAR_TIMEOUT )); then
      echo "ci-watch: no jobs for $sha"
      exit 2
    fi
    sleep "$POLL"
    continue
  fi
  failed="$(awk '$2 == "failure" || $2 == "cancelled" {print; exit}' <<<"$out")"
  summary="$(awk '{c[$2]++} END {for (s in c) printf "%s=%d ", s, c[s]}' <<<"$out")"
  printf '%s  %s\n' "$(date +%H:%M:%S)" "$summary"
  if [[ -n "$failed" ]]; then
    read -r id _ name <<<"$failed"
    explain "$id" "$name"
    exit 1
  fi
  if ! grep -qE ' (running|waiting|blocked|queued) ' <<<"$out"; then
    echo "ci-watch: all jobs finished without failure"
    exit 0
  fi
  $once && exit 0
  sleep "$POLL"
done
