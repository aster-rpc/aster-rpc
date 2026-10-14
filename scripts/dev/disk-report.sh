#!/usr/bin/env bash
# disk-report.sh -- read-only report of what build artifacts and worktrees are
# using disk under a dev root (default ~/dev/aster), and which of them are safe
# to remove. Deletes nothing: the cleanup steps are in
# docs/_internal/dev-loop-runbook.md §"Disk hygiene".
#
#   scripts/dev/disk-report.sh [root]
set -euo pipefail

root="${1:-$HOME/dev/aster}"

echo "== free space"
df -h "$HOME" | tail -1 | awk '{print "  " $4 " free of " $2 " (" $5 " used)"}'

echo
echo "== cargo/rustc processes (never clean a target while one is building it)"
pgrep -fl 'cargo |rustc ' | grep -v pgrep | cut -c1-140 | sed 's/^/  /' || echo "  none"

echo
echo "== orphaned test processes (parent gone; safe to kill once confirmed)"
ps -A -o pid=,ppid=,etime=,command= | awk '$2 == 1 && $4 ~ /\/target\/(debug|release)\// {print}' \
  | cut -c1-140 | sed 's/^/  /' | grep . || echo "  none"

echo
echo "== target directories (logical size; APFS may count reflinked data twice)"
find "$root" -maxdepth 5 -type d -name target -prune 2>/dev/null \
  | while read -r t; do
      [[ -f "$t/CACHEDIR.TAG" || -d "$t/debug" || -d "$t/release" ]] || continue
      kb=$(du -sk "$t" 2>/dev/null | cut -f1)
      age=$(( ( $(date +%s) - $(stat -f %m "$t") ) / 86400 ))
      printf '%8.1f GiB  %4sd  %s\n' "$(echo "$kb/1048576" | bc -l)" "$age" "$t"
    done | sort -rn | sed 's/^/  /'

echo
echo "== git worktrees (merged = every commit already in the main branch)"
seen_wt=" "
for repo in "$root"/*/; do
  [[ -e "$repo/.git" ]] || continue
  main=$(git -C "$repo" symbolic-ref --short refs/remotes/origin/HEAD 2>/dev/null || echo main)
  while read -r wt; do
        [[ "$seen_wt" == *" $wt "* ]] && continue
        seen_wt="$seen_wt$wt "
        if [[ ! -d "$wt" ]]; then
          echo "  PRUNABLE  $wt (directory gone: git -C $repo worktree prune)"
          continue
        fi
        branch=$(git -C "$wt" branch --show-current 2>/dev/null || true)
        dirty=$(git -C "$wt" status --porcelain 2>/dev/null | wc -l | tr -d ' ')
        # Patch-equivalent commits count as merged (rebased or cherry-picked).
        unmerged=$(git -C "$wt" cherry "$main" HEAD 2>/dev/null | grep -c '^+' || true)
        size=$(du -sh "$wt" 2>/dev/null | cut -f1)
        locked=""
        [[ -f "$(git -C "$wt" rev-parse --git-dir)/locked" ]] && locked=" LOCKED"
        state="safe"
        (( dirty > 0 )) && state="DIRTY($dirty)"
        (( unmerged > 0 )) && state="$state UNMERGED($unmerged)"
        printf '  %-22s %6s  %s [%s]%s\n' "$state" "$size" "$wt" "${branch:-detached}" "$locked"
      done < <(git -C "$repo" worktree list --porcelain 2>/dev/null | awk '/^worktree /{print $2}' | tail -n +2)
done

echo
echo "== kache store (bounded by local_max_size; GC is automatic)"
if command -v kache >/dev/null 2>&1; then
  kache stats 2>/dev/null | grep -iE 'size|dedup|entries' | sed 's/^/  /' || true
else
  echo "  kache not on PATH"
fi

echo
echo "== Mac Forge runner"
runner="$root/local_mac_forgejo_runner"
if [[ -d "$runner" ]]; then
  du -sh "$runner"/_work "$runner"/cache 2>/dev/null | sed 's/^/  /'
fi
