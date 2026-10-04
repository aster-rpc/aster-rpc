#!/usr/bin/env bash
# Publish staged npm package directories, in order, to one registry.
#
#   scripts/ci/npm-publish-all.sh <registry-url> <dist-tag> <order-file>
#
# <order-file> lists one package directory per line (dependencies first).
# Each package's publishConfig.registry is pointed at <registry-url> so the
# package cannot leak to another registry, and a version that already exists
# there is skipped, which makes a re-run of the same run number safe.
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
trap 'rm -f "$npmrc"' EXIT
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
  if npm view "$name@$version" version --registry "$registry" >/dev/null 2>&1; then
    log "$name@$version already on $registry; skipping"
    continue
  fi
  (cd "$dir" && npm publish --access public --tag "$dist_tag" --ignore-scripts --registry "$registry")
  log "published $name@$version ($dist_tag) to $registry"
done < "$order_file"
