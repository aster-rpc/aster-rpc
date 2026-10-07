#!/usr/bin/env bash
# Publish staged npm package directories, in order, to one registry.
#
#   scripts/ci/npm-publish-all.sh <registry-url> <dist-tag> <order-file>
#
# <order-file> lists one package directory per line (dependencies first).
# Each package's publishConfig.registry is pointed at <registry-url> so the
# package cannot leak to another registry. Existing versions must have the
# identical packed tarball checksum; a conflicting version is never skipped.
#
# Env:
#   NODE_AUTH_TOKEN  (required) token for <registry-url>. It is referenced from
#                    a temporary .npmrc as ${NODE_AUTH_TOKEN} (npm expands it),
#                    never written out or passed as an argument.
set -euo pipefail
IFS=$'\n\t'

log() { printf '%s npm-publish-all: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2; }

[[ $# -eq 3 ]] || { log "usage: npm-publish-all.sh <registry-url> <dist-tag> <order-file>"; exit 64; }
registry="$1"; dist_tag="$2"; order_file="$3"
[[ "$registry" =~ ^https://[^[:space:]]+/$ ]] || { log "registry must be an https URL ending in /"; exit 64; }
[[ "$dist_tag" =~ ^[a-z][a-z0-9-]*$ ]] || { log "invalid dist-tag '$dist_tag'"; exit 64; }
[[ -f "$order_file" ]] || { log "missing order file $order_file"; exit 66; }
: "${NODE_AUTH_TOKEN:?NODE_AUTH_TOKEN is required}"

repo="$(cd "$(dirname "$0")/../.." && pwd)"
npmrc="$(mktemp)"
pack_dir="$(mktemp -d)"
trap 'rm -f "$npmrc"; rm -rf "$pack_dir"' EXIT
# `//host/path/:_authToken` keys the token to this registry only.
auth_key="${registry#https:}"
# shellcheck disable=SC2016 # npm expands ${NODE_AUTH_TOKEN} itself
printf '@aster-rpc:registry=%s\n%s:_authToken=${NODE_AUTH_TOKEN}\n' "$registry" "$auth_key" > "$npmrc"
export NPM_CONFIG_USERCONFIG="$npmrc"

while IFS= read -r dir; do
  [[ -n "$dir" ]] || continue
  [[ -f "$dir/package.json" ]] || { log "missing $dir/package.json"; exit 1; }
  name="$(node -p "require('./$dir/package.json').name")"
  version="$(node -p "require('./$dir/package.json').version")"
  node "$repo/scripts/ci/stamp-npm-package.mjs" "$dir/package.json" "$version" "$registry" >/dev/null
  # Packing is serialization of the verified candidate directory, with no
  # build/lifecycle scripts. Publish this very tarball, then verify it against
  # the registry's checksum; npm publish must not pack a second copy.
  packed=$(cd "$dir" && npm pack --ignore-scripts --json --pack-destination "$pack_dir")
  file=$(node -e 'console.log(JSON.parse(process.argv[1])[0].filename)' "$packed")
  local_sha=$(node -e 'console.log(JSON.parse(process.argv[1])[0].shasum)' "$packed")
  if remote=$(npm view "$name@$version" dist.shasum --json --registry "$registry" 2>"$pack_dir/view-error"); then
    remote_sha=$(node -e 'console.log(JSON.parse(process.argv[1]))' "$remote")
    [[ "$remote_sha" == "$local_sha" ]] || { log "immutable $name@$version already exists with another checksum"; exit 1; }
    log "$name@$version already on $registry with identical bytes; skipping"
    continue
  fi
  # Authentication, transport and server errors must not be treated as 404.
  # Some npm launchers append a diagnostic after npm's JSON object. Decode
  # that first structured response; never infer absence from free-form text.
  if ! python3 -c 'import json,sys; response,_ = json.JSONDecoder().raw_decode(sys.stdin.read()); sys.exit(0 if response.get("error",{}).get("code") == "E404" else 1)' <<<"$remote"; then
    cat "$pack_dir/view-error" >&2
    log "could not verify $name@$version on $registry"; exit 1
  fi
  npm publish "$pack_dir/$file" --access public --tag "$dist_tag" --ignore-scripts --registry "$registry"
  published=$(npm view "$name@$version" dist.shasum --json --registry "$registry")
  published_sha=$(node -e 'console.log(JSON.parse(process.argv[1]))' "$published")
  [[ "$published_sha" == "$local_sha" ]] || { log "publication checksum mismatch for $name@$version"; exit 1; }
  log "published $name@$version ($dist_tag) to $registry"
done < "$order_file"
