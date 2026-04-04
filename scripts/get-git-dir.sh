#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  get-git-dir.sh <remote_repo> <remote_dir> <destination_dir> [branch] [commit]

Examples:
  get-git-dir.sh "https://github.com/apache/fory-site.git" "docs" "fory-docs"
  get-git-dir.sh "https://github.com/apache/fory-site.git" "docs" "fory-docs" "main"
  get-git-dir.sh "https://github.com/apache/fory-site.git" "docs" "fory-docs" "main" "abc1234"
EOF
  exit 1
}

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Error: required command not found: $1" >&2
    exit 1
  }
}

read_source_info_value() {
  local key="$1"
  local file="$2"
  [[ -f "$file" ]] || return 0
  awk -F= -v k="$key" '$1 == k { sub(/^[^=]*=/, ""); print; exit }' "$file"
}

normalize_repo_path() {
  local p="$1"
  p="${p#./}"
  p="${p#/}"
  p="${p%/}"
  printf '%s' "$p"
}

require_cmd git
require_cmd rsync
require_cmd awk
require_cmd mktemp
require_cmd date

[[ $# -lt 3 || $# -gt 5 ]] && usage

REMOTE_REPO="$1"
REMOTE_DIR="$(normalize_repo_path "$2")"
DEST_DIR="$3"
BRANCH="${4:-main}"
PIN_REF="${5:-}"

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ABS_DEST_DIR="$(mkdir -p "$(dirname "$DEST_DIR")" && cd "$(dirname "$DEST_DIR")" && pwd)/$(basename "$DEST_DIR")"
SOURCE_INFO_FILE="$ABS_DEST_DIR/.source-info"

CURRENT_COMMIT="$(read_source_info_value resolved_commit "$SOURCE_INFO_FILE")"
CURRENT_TREE="$(read_source_info_value resolved_tree "$SOURCE_INFO_FILE")"

WORK_REPO="$TMP_DIR/repo"
git init --quiet "$WORK_REPO"
git -C "$WORK_REPO" remote add origin "$REMOTE_REPO"

if [[ -n "$PIN_REF" ]]; then
  echo "Resolving pinned ref: $PIN_REF"
  git -C "$WORK_REPO" fetch --quiet --depth=1 origin "$PIN_REF"
else
  echo "Resolving branch: $BRANCH"
  git -C "$WORK_REPO" fetch --quiet --depth=1 origin "$BRANCH"
fi

REMOTE_COMMIT="$(git -C "$WORK_REPO" rev-parse FETCH_HEAD)"

if ! REMOTE_TREE="$(git -C "$WORK_REPO" rev-parse "FETCH_HEAD:$REMOTE_DIR" 2>/dev/null)"; then
  echo "Error: directory '$REMOTE_DIR' was not found at the selected revision" >&2
  exit 1
fi

# Sanity-check that the resolved object is a tree.
REMOTE_TREE_TYPE="$(git -C "$WORK_REPO" cat-file -t "$REMOTE_TREE")"
if [[ "$REMOTE_TREE_TYPE" != "tree" ]]; then
  echo "Error: '$REMOTE_DIR' exists but is not a directory at the selected revision" >&2
  exit 1
fi

if [[ -n "$CURRENT_TREE" && "$CURRENT_TREE" == "$REMOTE_TREE" ]]; then
  echo "No change in '$REMOTE_DIR'."
  echo "Destination dir: $ABS_DEST_DIR"
  echo "Resolved tree:   $CURRENT_TREE"
  echo "Resolved commit: ${CURRENT_COMMIT:-unknown}"
  exit 0
fi

echo "Change detected."
echo "Previous tree: ${CURRENT_TREE:-<none>}"
echo "Remote tree:   $REMOTE_TREE"

git -C "$WORK_REPO" sparse-checkout init --cone
git -C "$WORK_REPO" sparse-checkout set "$REMOTE_DIR"
git -C "$WORK_REPO" checkout --quiet FETCH_HEAD

mkdir -p "$ABS_DEST_DIR"
rsync -a --delete "$WORK_REPO/$REMOTE_DIR"/ "$ABS_DEST_DIR"/

SYNCED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

cat > "$SOURCE_INFO_FILE" <<EOF
remote_repo=$REMOTE_REPO
remote_dir=$REMOTE_DIR
branch=$BRANCH
pinned_ref=${PIN_REF:-}
resolved_commit=$REMOTE_COMMIT
resolved_tree=$REMOTE_TREE
synced_at_utc=$SYNCED_AT
EOF

echo "Done."
echo "Repo:            $REMOTE_REPO"
echo "Source dir:      $REMOTE_DIR"
echo "Destination dir: $ABS_DEST_DIR"
echo "Resolved commit: $REMOTE_COMMIT"
echo "Resolved tree:   $REMOTE_TREE"
echo "Metadata file:   $SOURCE_INFO_FILE"