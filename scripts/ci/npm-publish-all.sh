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
exec python3 "$repo/scripts/ci/npm-publish-all.py" "$registry" "$dist_tag" "$order_file"
