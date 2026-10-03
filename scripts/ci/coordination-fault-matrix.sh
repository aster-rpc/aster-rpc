#!/usr/bin/env bash
# Run the §11.4 coordination process-death matrix on the native host and prove
# it actually ran. Shared by the macOS and Windows CI jobs so their filter,
# floor and by-name checks cannot drift apart.
#
# Gate: on a push to `main` the matrix runs only when the push touches what it
# exercises (coordination code, its dependencies, the toolchain, or this job).
# Tags, manual runs and anything else always run it -- the tag run is the
# release gate. Inputs come from the workflow:
#
#   CI_EVENT   forgejo.event_name
#   CI_REF     forgejo.ref
#   CI_BEFORE  forgejo.event.before (the pushed range's base; may be empty or zeros)
#
# Needs full history (`fetch-depth: 0`) for the range diff. A base it cannot
# resolve runs the matrix rather than guessing.
set -euo pipefail

platform="$(uname -s)"

if [[ "${CI_EVENT:-}" == "push" && "${CI_REF:-}" == "refs/heads/main" ]]; then
  before="${CI_BEFORE:-}"
  if [[ -n "$before" && ! "$before" =~ ^0+$ ]] \
     && git cat-file -e "${before}^{commit}" 2>/dev/null; then
    relevant=$(git diff --name-only "$before" HEAD | grep -E \
      '^(aster/src/coordination/|aster/Cargo\.toml$|Cargo\.toml$|Cargo\.lock$|rust-toolchain(\.toml)?$|\.forgejo/workflows/linux\.yml$|scripts/ci/coordination-fault-matrix\.sh$)' \
      || true)
    if [[ -z "$relevant" ]]; then
      echo "coordination fault matrix: skipped on $platform -- ${before:0:12}..HEAD touches nothing it exercises (tags always run it)"
      exit 0
    fi
    echo "coordination fault matrix: running on $platform for:"
    echo "$relevant" | sed 's/^/  /'
  else
    echo "coordination fault matrix: push base '${before}' is unusable; running"
  fi
fi

# Single-threaded: each case spawns a child that opens the same storage
# directory the parent prepared, and a directory lock is a directory lock.
out=$(cargo test --locked -p aster --lib \
  --no-default-features --features coordination \
  -- --test-threads=1 \
  coordination::storage::crash:: \
  every_boundary_recovers_to_a_state_the_protocol_permits \
  every_boundary_is_covered_by_some_operation \
  no_operation_declares_a_boundary_twice \
  the_crash_matrix_inventory_is_complete \
  no_cell_settles_for_an_undetermined_outcome \
  the_openraft_storage_suite_passes \
  a_normal_command_and_a_non_empty_log_survive_a_restart 2>&1) || {
    echo "$out"; exit 1; }
echo "$out"
# Windows cargo output carries CRLF; strip it before matching.
out=${out//$'\r'/}

passed=$(echo "$out" | sed -n 's/^test result: ok\. \([0-9]*\) passed.*/\1/p' | head -1)
# A filter that matches nothing reports "ok. 0 passed", which is not evidence of
# anything. The boundary declaration, the matrix and its inventory, and both
# suite runs are twelve between them, so anything less means the filter stopped
# matching (it did once: a rename at 33 -> 36 cells dropped the count to eleven).
if [[ -z "$passed" || "$passed" -lt 12 ]]; then
  echo "::error::the coordination fault matrix reported '$passed' passing tests on $platform; \
expected at least twelve, so the filter stopped matching"
  exit 1
fi
# "Enough tests ran" is not "this test ran". The two gates that carry the matrix
# itself are required by name, so a rename that outruns this file fails here
# instead of being absorbed by the total.
for name in the_crash_matrix_inventory_is_complete \
            every_boundary_recovers_to_a_state_the_protocol_permits; do
  if ! echo "$out" | grep -qE "^test .*${name} \.\.\. ok$"; then
    echo "::error::the fault matrix never reported '$name' as passing on $platform; \
it is named in the filter, so it was either renamed without updating this file or has stopped running"
    exit 1
  fi
done
echo "coordination fault matrix: $passed tests passed on native $platform, \
including the inventory and the matrix itself"
