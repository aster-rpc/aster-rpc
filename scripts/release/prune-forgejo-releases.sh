#!/usr/bin/env bash
# Keep only the newest Forgejo releases of Aster/aster-rpc-internal.
#
#   scripts/release/prune-forgejo-releases.sh [--dry-run]
#
# Releases are the maintainer record (wheels, sdist, SHA256SUMS); consumers
# install from the Cargo registry and the public generic packages, neither of
# which this touches. Deleting a Forgejo release keeps its git tag, so every
# version stays reproducible from source.
#
# Only non-draft releases tagged vMAJOR.MINOR.PATCH are candidates, ranked by
# version (not creation time). ASTER_RELEASE_KEEP sets how many survive
# (default 3). ASTER_RELEASE_TOKEN needs release write.
set -euo pipefail

: "${ASTER_RELEASE_TOKEN:?ASTER_RELEASE_TOKEN is required}"
keep="${ASTER_RELEASE_KEEP:-3}"
dry_run=0
[[ "${1:-}" == "--dry-run" ]] && dry_run=1
[[ "$keep" =~ ^[1-9][0-9]*$ ]] || { echo "error: ASTER_RELEASE_KEEP must be a positive integer" >&2; exit 1; }

forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
api="$forge/api/v1/repos/Aster/aster-rpc-internal"
auth=(-H "Authorization: token $ASTER_RELEASE_TOKEN")
request() {
  local method="$1" path="$2"; shift 2
  curl --fail-with-body --silent --show-error -X "$method" "${auth[@]}" "$@" "$api$path"
}

releases="[]"
page=1
while :; do
  batch="$(request GET "/releases?draft=false&limit=50&page=$page")"
  count="$(python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' <<<"$batch")"
  releases="$(python3 -c 'import json,sys; a=json.loads(sys.argv[1]); a+=json.load(sys.stdin); print(json.dumps(a))' "$releases" <<<"$batch")"
  (( count < 50 )) && break
  page=$((page + 1))
done

# "<id> <tag>" for each release to delete, oldest last.
doomed="$(RELEASES_JSON="$releases" python3 - "$keep" <<'PY'
import json, os, re, sys
keep = int(sys.argv[1])
pattern = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")
ranked = []
for release in json.loads(os.environ["RELEASES_JSON"]):
    match = pattern.match(release["tag_name"])
    if release.get("draft") or not match:
        continue
    ranked.append((tuple(int(part) for part in match.groups()), release["id"], release["tag_name"]))
ranked.sort(reverse=True)
for version, release_id, tag in ranked[:keep]:
    print(f"keep {tag}", file=sys.stderr)
for version, release_id, tag in ranked[keep:]:
    print(release_id, tag)
PY
)"

if [[ -z "$doomed" ]]; then
  echo "nothing to prune (keeping the newest $keep)"
  exit 0
fi
while read -r release_id tag; do
  if (( dry_run )); then
    echo "would delete release $tag (id $release_id); tag kept"
  else
    request DELETE "/releases/$release_id" >/dev/null
    echo "deleted release $tag (id $release_id); tag kept"
  fi
done <<<"$doomed"
