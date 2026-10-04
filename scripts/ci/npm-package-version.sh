#!/usr/bin/env bash
# Print the npm version and dist-tag for this CI run as GITHUB_ENV lines:
#
#   scripts/ci/npm-package-version.sh >> "$GITHUB_ENV"
#
#   ASTER_BUILD_VERSION  scripts/release/version.sh
#   NPM_VERSION          <version> on a v<version> tag, else <version>-dev.<run>
#   NPM_DIST_TAG         latest on a tag, else dev
#
# A v* tag that does not match the derived version fails, as the other
# release scripts do. Needs full history (`fetch-depth: 0`).
set -euo pipefail
IFS=$'\n\t'

repo="$(cd "$(dirname "$0")/../.." && pwd)"
version="$("$repo/scripts/release/version.sh")"
ref="${FORGEJO_REF:-${GITHUB_REF:-}}"
ref_name="${FORGEJO_REF_NAME:-${GITHUB_REF_NAME:-}}"
run="${FORGEJO_RUN_NUMBER:-${GITHUB_RUN_NUMBER:-}}"

if [[ "$ref" == refs/tags/v* ]]; then
  if [[ "$ref_name" != "v$version" ]]; then
    echo "error: tag $ref_name does not match derived version v$version" >&2
    exit 1
  fi
  npm_version="$version"
  dist_tag=latest
else
  [[ "$run" =~ ^[0-9]+$ ]] || { echo "error: run number '$run' is not numeric" >&2; exit 1; }
  npm_version="$version-dev.$run"
  dist_tag=dev
fi

printf 'ASTER_BUILD_VERSION=%s\nNPM_VERSION=%s\nNPM_DIST_TAG=%s\n' \
  "$version" "$npm_version" "$dist_tag"
