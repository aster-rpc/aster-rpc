#!/usr/bin/env bash
# Tag the current main commit with its derived Aster version and push the tag.
# The Forgejo tag workflow builds and publishes the permanent release assets.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
allow_dirty=false
push=true
for arg in "$@"; do
  case "$arg" in
    --allow-dirty) allow_dirty=true ;;
    --no-push) push=false ;;
    *) echo "usage: scripts/release.sh [--allow-dirty] [--no-push]" >&2; exit 2 ;;
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
if git -C "$repo" rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  tagged="$(git -C "$repo" rev-list -n 1 "$tag")"
  head="$(git -C "$repo" rev-parse HEAD)"
  [[ "$tagged" == "$head" ]] || {
    echo "error: $tag already points to $tagged" >&2
    exit 1
  }
  echo "$tag already tags HEAD"
else
  git -C "$repo" tag -a "$tag" -m "Aster $version"
  echo "created $tag at $(git -C "$repo" rev-parse --short HEAD)"
fi

if [[ "$push" == true ]]; then
  git -C "$repo" push origin "refs/tags/$tag"
  echo "pushed $tag; Forgejo will publish the release after CI passes"
else
  echo "tag not pushed (--no-push)"
fi
