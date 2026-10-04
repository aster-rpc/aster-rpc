#!/usr/bin/env bash
# Attach files to the Forgejo release for an existing tag, creating the release
# if it does not exist yet.
#
#   scripts/release/attach-forgejo-release.sh <tag> <title> <file>...
#
# Several tag workflows attach to one release (`v*`: Python wheels from
# linux.yml via publish-forgejo.sh, FFI natives and binding packages from
# ffi-bindings.yml and typescript-packages.yml; `cli-v*`: cli.yml). They run
# concurrently, so creation tolerates losing the race: a failed create is
# followed by a second lookup. Assets with the same name are replaced.
#
# Unlike publish-forgejo.sh this does not require every asset to be a Python
# distribution, so it can carry native libraries and binding packages.
#
# Env:
#   ASTER_RELEASE_TOKEN  (required) token with repository release write
#   FORGEJO_SERVER_URL   (default: https://forge.emrul.dev)
#   FORGEJO_REPOSITORY   (default: Aster/aster-rpc-internal)
#
# The token reaches curl through a config on standard input, never argv.
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s attach-forgejo-release: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "error: $*"; exit "${2:-1}"; }

[[ $# -ge 3 ]] || die "usage: attach-forgejo-release.sh <tag> <title> <file>..." 64
tag="$1"; title="$2"; shift 2
[[ "$tag" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || die "invalid tag '$tag'" 64
: "${ASTER_RELEASE_TOKEN:?ASTER_RELEASE_TOKEN is required}"
for file in "$@"; do
  [[ -f "$file" ]] || die "not a file: $file" 66
done

forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"
repository="${FORGEJO_REPOSITORY:-Aster/aster-rpc-internal}"
api="$forge/api/v1/repos/$repository"

request() {
  local method="$1" path="$2"; shift 2
  printf 'header = "Authorization: token %s"\n' "$ASTER_RELEASE_TOKEN" \
    | curl --config - --fail-with-body --silent --show-error -X "$method" "$@" "$api$path"
}

json_field() {
  python3 -c 'import json, sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
}

lookup_release() {
  request GET "/releases/tags/$tag" 2>/dev/null | json_field id 2>/dev/null || true
}

release_id="$(lookup_release)"
if [[ -z "$release_id" ]]; then
  payload="$(python3 - "$tag" "$title" <<'PY'
import json, sys
tag, title = sys.argv[1:3]
print(json.dumps({
    "tag_name": tag,
    "name": title,
    "body": f"{title}. Assets are attached by the Forgejo tag workflows.",
    "draft": False,
    "prerelease": False,
}))
PY
)"
  payload_file="$(mktemp)"
  trap 'rm -f "$payload_file"' EXIT
  printf '%s' "$payload" > "$payload_file"
  if ! release_id="$(request POST /releases -H 'Content-Type: application/json' \
      --data-binary "@$payload_file" | json_field id)"; then
    log "create failed; another tag workflow may have created it first"
    release_id="$(lookup_release)"
  fi
fi
[[ "$release_id" =~ ^[0-9]+$ ]] || die "could not find or create the release for $tag" 3

for file in "$@"; do
  name="$(basename "$file")"
  asset_id="$(request GET "/releases/$release_id/assets" | python3 -c '
import json, sys
name = sys.argv[1]
print(next((str(a["id"]) for a in json.load(sys.stdin) if a["name"] == name), ""))
' "$name")"
  if [[ -n "$asset_id" ]]; then
    request DELETE "/releases/$release_id/assets/$asset_id" >/dev/null
  fi
  request POST "/releases/$release_id/assets?name=$name" \
    -F "attachment=@$file;type=application/octet-stream" >/dev/null
  log "attached $name to $tag"
done
log "release $forge/$repository/releases/tag/$tag"
