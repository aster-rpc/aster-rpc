#!/usr/bin/env bash
# Qualify the current main commit as a release candidate, then tag it.
#
#   1. HEAD must be main, pushed, and the tip of origin/main.
#   2. Dispatch linux.yml on main with sha=HEAD (a push runs only the gate;
#      the dispatch runs every gate again plus every release producer and
#      seals the candidate). An existing candidate run for HEAD is reused;
#      a failed or duplicate one cannot qualify (fix and commit).
#   3. Follow it with scripts/ci/ci-watch.sh; tag only when it succeeded.
#   4. Push v<version>; release.yml promotes the sealed candidate's bytes.
#
# Reads and dispatches with the git credential for forge.emrul.dev, like
# ci-watch.sh. See docs/_internal/dev-loop-runbook.md, "Cutting a release".
set -euo pipefail

FORGE="${ASTER_FORGE_URL:-https://forge.emrul.dev}"
REPO="${ASTER_FORGE_REPO:-Aster/aster-rpc-internal}"
WORKFLOW=linux.yml

repo="$(cd "$(dirname "$0")/.." && pwd)"
allow_dirty=false
push=true
candidate_only=false
for arg in "$@"; do
  case "$arg" in
    --allow-dirty) allow_dirty=true ;;
    --no-push) push=false ;;
    # Qualify HEAD (every producer, sealed candidate) without tagging it.
    --candidate-only) candidate_only=true ;;
    *) echo "usage: scripts/release.sh [--allow-dirty] [--no-push] [--candidate-only]" >&2; exit 2 ;;
  esac
done

version="$("$repo/scripts/release/version.sh")"
tag="v$version"
branch="$(git -C "$repo" branch --show-current)"
[[ "$branch" == main ]] || {
  echo "error: releases must be tagged from main (currently '$branch')" >&2
  exit 1
}
if [[ "$allow_dirty" != true && -n "$(git -C "$repo" status --porcelain)" ]]; then
  echo "error: worktree is dirty; commit/stash it or pass --allow-dirty" >&2
  exit 1
fi
head="$(git -C "$repo" rev-parse HEAD)"

if git -C "$repo" rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  tagged="$(git -C "$repo" rev-list -n 1 "$tag")"
  [[ "$tagged" == "$head" ]] || {
    echo "error: $tag already points to $tagged" >&2
    exit 1
  }
  echo "$tag already tags HEAD"
  if [[ "$candidate_only" == true ]]; then exit 0; fi
else
  # The dispatch runs whatever origin/main points at; it must be this commit.
  git -C "$repo" fetch -q origin main
  remote="$(git -C "$repo" rev-parse origin/main)"
  [[ "$remote" == "$head" ]] || {
    echo "error: HEAD $head is not origin/main ($remote); push it (or pull) first" >&2
    exit 1
  }

  token="$(printf 'protocol=https\nhost=%s\n\n' "${FORGE#https://}" | git credential fill 2>/dev/null | sed -n 's/^password=//p')"
  [[ -n "$token" ]] || { echo "error: no git credential for $FORGE" >&2; exit 1; }
  api() { curl -fsS -H "Authorization: token $token" -H "Content-Type: application/json" "$@"; }

  # "<id> <status>" for each candidate dispatch run of HEAD on main.
  candidates() {
    api "$FORGE/api/v1/repos/$REPO/actions/runs?event=workflow_dispatch&workflow_id=$WORKFLOW&head_sha=$head&ref=refs/heads/main&limit=50" |
      python3 -c '
import json, sys
sha, workflow = sys.argv[1], sys.argv[2]
for r in json.load(sys.stdin).get("workflow_runs", []):
    if (r.get("commit_sha") == sha and r.get("workflow_id") == workflow
            and r.get("event") == "workflow_dispatch" and r.get("prettyref") in ("main", "refs/heads/main")):
        print(r["id"], r["status"])
' "$head" "$WORKFLOW"
  }

  existing="$(candidates)"
  if [[ -z "$existing" ]]; then
    body="$(python3 -c 'import json, sys; print(json.dumps({"ref": "main", "inputs": {"sha": sys.argv[1]}, "return_run_info": True}))' "$head")"
    api -X POST -d "$body" "$FORGE/api/v1/repos/$REPO/actions/workflows/$WORKFLOW/dispatches" >/dev/null
    echo "dispatched release candidate for $(git -C "$repo" log -1 --format='%h %s' "$head")"
  elif [[ "$(wc -l <<<"$existing")" -ne 1 ]]; then
    echo "error: $head already has several candidate runs; promotion refuses it. Fix and commit:" >&2
    echo "$existing" >&2
    exit 1
  else
    read -r run_id status <<<"$existing"
    case "$status" in
      success|running|waiting|blocked|queued) echo "reusing candidate run $run_id ($status)" ;;
      *) echo "error: candidate run $run_id for $head is $status; no retry-to-green. Fix and commit." >&2; exit 1 ;;
    esac
  fi

  # Exit 0 only when every candidate job that ran succeeded.
  "$repo/scripts/ci/ci-watch.sh" "$head" --event workflow_dispatch || {
    echo "error: the release candidate for $head did not succeed; not tagging" >&2
    exit 1
  }
  final="$(candidates)"
  [[ "$final" =~ ^[0-9]+\ success$ ]] || {
    echo "error: expected one successful candidate run for $head, found: ${final:-none}" >&2
    exit 1
  }
  echo "release candidate run ${final% *} succeeded and sealed $head"
  if [[ "$candidate_only" == true ]]; then
    echo "not tagging (--candidate-only); rerun without it to release $tag"
    exit 0
  fi

  git -C "$repo" tag -a "$tag" -m "Aster $version"
  echo "created $tag at $(git -C "$repo" rev-parse --short HEAD)"
fi

if [[ "$push" == true ]]; then
  git -C "$repo" push origin "refs/tags/$tag"
  echo "pushed $tag; release.yml promotes the sealed candidate for this commit"
else
  echo "tag not pushed (--no-push)"
fi
