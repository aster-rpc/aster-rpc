#!/usr/bin/env bash
# Upload files to one version of a Forgejo generic package, idempotently.
#
#   scripts/release/publish-generic-package.sh <package> <version> <file>...
#
# Used only for tagged releases (`<package>/<semver>`); publishing happens on
# versioned release tags, never on branch pushes. A file already present with the same SHA-256 is skipped; one
# present with a different SHA-256 fails the run rather than being replaced, so
# a published version is immutable. Forgejo's own digests are re-read after
# upload, so a 2xx response is not the only evidence.
#
# Env:
#   ASTER_PACKAGE_TOKEN   (required) Forgejo token with write:package
#   ASTER_PACKAGE_USER    account that owns the token (default: agents)
#   ASTER_PACKAGE_OWNER   package owner (default: Aster)
#   FORGEJO_SERVER_URL    (default: https://forge.emrul.dev)
#
# The token reaches curl through a config on standard input, never argv.
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s publish-generic-package: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }
die() { log "error: $*"; exit "${2:-1}"; }

[[ $# -ge 3 ]] || die "usage: publish-generic-package.sh <package> <version> <file>..." 64
package="$1"; version="$2"; shift 2
[[ "$package" =~ ^[a-z0-9][a-z0-9._-]*$ ]] || die "invalid package name '$package'" 64
[[ "$version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || die "invalid version '$version'" 64
: "${ASTER_PACKAGE_TOKEN:?ASTER_PACKAGE_TOKEN is required}"
user="${ASTER_PACKAGE_USER:-agents}"
owner="${ASTER_PACKAGE_OWNER:-Aster}"
forge="${FORGEJO_SERVER_URL:-https://forge.emrul.dev}"

for file in "$@"; do
  [[ -f "$file" ]] || die "not a file: $file" 66
done

files_api="$forge/api/v1/packages/$owner/generic/$package/$version/files"
upload_base="$forge/api/packages/$owner/generic/$package/$version"

# API calls get 180 s. An upload gets 180 s plus 1 s per MiB: a candidate
# bundle is several hundred MiB, and a runner on a home uplink (linbox) cannot
# push that in 180 s (release candidate run 281 timed out at exactly 180 s).
authed_curl() {
  printf 'user = "%s:%s"\n' "$user" "$ASTER_PACKAGE_TOKEN" \
    | curl --config - --silent --show-error --connect-timeout 10 \
      --max-time "${max_time:-180}" "$@"
}

upload_max_time() {
  local bytes
  bytes="$(wc -c <"$1" | tr -d ' ')"
  echo $((180 + bytes / 1048576))
}

remote_files() {
  # A version that does not exist yet answers 404; that is an empty listing.
  # Authentication, proxy and transport failures must not authorize writes.
  local response status
  response="$(authed_curl --write-out $'\n%{http_code}' "$files_api")" || return 1
  status="${response##*$'\n'}"
  case "$status" in
    200) printf '%s' "${response%$'\n'*}" ;;
    404) printf '[]' ;;
    *) die "cannot inspect existing package files: HTTP $status" ;;
  esac
}

remote_sha256() {
  python3 -c '
import json, sys
name = sys.argv[1]
print(next((f["sha256"] for f in json.load(sys.stdin) if f["name"] == name), ""))
' "$1"
}

sha256_of() {
  python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"
}

listing="$(remote_files)"
for file in "$@"; do
  name="$(basename "$file")"
  local_digest="$(sha256_of "$file")"
  published_digest="$(remote_sha256 "$name" <<<"$listing")"
  if [[ -n "$published_digest" ]]; then
    [[ "$published_digest" == "$local_digest" ]] \
      || die "$package/$version/$name already exists with another checksum" 3
    log "$package/$version/$name already published; skipping"
    continue
  fi
  max_time="$(upload_max_time "$file")" \
    authed_curl --fail-with-body --upload-file "$file" "$upload_base/$name" >/dev/null
  log "uploaded $package/$version/$name"
done

listing="$(remote_files)"
for file in "$@"; do
  name="$(basename "$file")"
  [[ "$(remote_sha256 "$name" <<<"$listing")" == "$(sha256_of "$file")" ]] \
    || die "verification failed for $package/$version/$name" 4
done
log "published $forge/$owner/-/packages/generic/$package/$version"
