#!/usr/bin/env bash
# Print `true` when the change under test touches a path matching <ERE>,
# otherwise `false`. Replaces GitHub's dorny/paths-filter for Forgejo jobs that
# should run only when particular inputs change.
#
#   scripts/ci/paths-changed.sh '<extended regex over repo-relative paths>'
#
# Inputs (from the workflow):
#   CI_EVENT   forgejo.event_name
#   CI_REF     forgejo.ref
#   CI_BEFORE  forgejo.event.before (push base; may be empty or zeros)
#   CI_BASE    forgejo.event.pull_request.base.sha (pull requests)
#
# Tags, manual runs and anything whose base cannot be resolved answer `true`:
# when in doubt, run. Needs full history (`fetch-depth: 0`).
set -euo pipefail
IFS=$'\n\t'

pattern="${1:?usage: paths-changed.sh '<extended regex>'}"

base=""
case "${CI_EVENT:-}" in
  push)
    if [[ "${CI_REF:-}" == refs/heads/* ]]; then
      base="${CI_BEFORE:-}"
    fi
    ;;
  pull_request)
    base="${CI_BASE:-}"
    if [[ -n "$base" ]] && git cat-file -e "${base}^{commit}" 2>/dev/null; then
      base="$(git merge-base "$base" HEAD 2>/dev/null || printf '%s' "$base")"
    fi
    ;;
esac

if [[ -z "$base" || "$base" =~ ^0+$ ]] || ! git cat-file -e "${base}^{commit}" 2>/dev/null; then
  echo "paths-changed: no usable base for event '${CI_EVENT:-}'; assuming relevant" >&2
  echo true
  exit 0
fi

set +e
matches="$(git diff --name-only "$base" HEAD | grep -E -- "$pattern")"
status=$?
set -e
# grep: 0 = matched, 1 = no match, >1 = a real error that must not read as "no".
if (( status > 1 )); then
  echo "paths-changed: grep failed with status $status" >&2
  exit 2
fi
if [[ -n "$matches" ]]; then
  echo "paths-changed: relevant changes since ${base:0:12}:" >&2
  printf '%s\n' "$matches" | sed 's/^/  /' >&2
  echo true
else
  echo "paths-changed: nothing relevant since ${base:0:12}" >&2
  echo false
fi
