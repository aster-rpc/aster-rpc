#!/usr/bin/env bash
# host-load.sh -- the load policy for jobs on the shared Linux runner hosts,
# and the evidence of it (docs/_internal/cicd.md, "Shared-host load").
#
#   scripts/ci/host-load.sh setup           once per job, after checkout: cap
#                                           Cargo's compile parallelism for the
#                                           job's later steps ($GITHUB_ENV)
#   scripts/ci/host-load.sh start|end [rc]  one line of evidence: nproc, load
#                                           average, CPU pressure, memory
#
# Each aster-forge-linux-x64 host runs two job containers at once. A compile
# at full parallelism next to a test job stretched that job's
# must-happen waits past their bounds, so compiles leave a quarter of the CPUs
# free: CARGO_BUILD_JOBS = nproc - nproc/4 (at least 1), unless the job
# already set it. `nice` would not do this: each job container is its own
# cgroup, and niceness only orders tasks within one.
#
# A test step logs `start` first and `end` from an EXIT trap, so a failure
# shows the load it ran under:
#
#   scripts/ci/host-load.sh start
#   trap 'scripts/ci/host-load.sh end "$?"' EXIT
#
# Evidence never fails a step: `start` and `end` always exit 0.
set -euo pipefail
IFS=$'\n\t'

cpus() { nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1; }

evidence() {
  local phase="$1" rc="${2:-}" load psi mem cg=""
  load="$(cut -d' ' -f1-3 /proc/loadavg 2>/dev/null || echo unknown)"
  # PSI "some avg10": the share of the last 10 s in which a runnable task waited for a CPU.
  psi="$(awk '/^some/ {sub("avg10=", "", $2); print $2 "%"}' /proc/pressure/cpu 2>/dev/null || true)"
  mem="$(awk '/^MemTotal:/ {t=$2} /^MemAvailable:/ {a=$2} END {if (t) printf "%.1f/%.1fGiB", a/1048576, t/1048576}' \
    /proc/meminfo 2>/dev/null || true)"
  if [[ -r /sys/fs/cgroup/memory.current ]]; then
    cg=" job_mem=$(awk '{printf "%.1fGiB", $1/1073741824}' /sys/fs/cgroup/memory.current)"
  fi
  printf '%s host-load %s%s: nproc=%s load=%s cpu_psi=%s mem_avail=%s%s cargo_jobs=%s scale=%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$phase" "${rc:+ rc=$rc}" "$(cpus)" "${load// /,}" \
    "${psi:-n/a}" "${mem:-n/a}" "$cg" "${CARGO_BUILD_JOBS:-default}" "${ASTER_TEST_TIME_SCALE:-1}"
}

case "${1:-}" in
  setup)
    n="$(cpus)"
    jobs="${CARGO_BUILD_JOBS:-$(( n - n / 4 ))}"
    (( jobs >= 1 )) || jobs=1
    if [[ -n "${GITHUB_ENV:-}" ]]; then
      echo "CARGO_BUILD_JOBS=$jobs" >> "$GITHUB_ENV"
    fi
    CARGO_BUILD_JOBS="$jobs" evidence setup ;;
  start|end)
    evidence "$1" "${2:-}" || true ;;
  *)
    sed -n '2,8p' "$0" >&2
    exit 2 ;;
esac
