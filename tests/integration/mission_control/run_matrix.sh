#!/bin/bash
# run_matrix.sh — Mission Control guide integration test matrix.
#
# Runs every combination of (server lang × client lang × mode) and reports
# a final pass/fail tally.
#
# Usage:
#   ./run_matrix.sh                    # full matrix, verbose
#   ./run_matrix.sh -q                 # full matrix, quiet (summary only)
#   ./run_matrix.sh --only py-py-dev   # single combo
#
# Working directory: tests/integration/mission_control/.work/
# - server.log, server.addr, server.pid for each running server
# - root.key, root.pub, edge.cred, ops.cred for auth tests
# - generated/ for gen-client output
#
# On full success the .work directory is deleted. On failure it's preserved
# so you can inspect server.log etc.

set -uo pipefail

# Resolve repo root no matter where this is invoked from
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
WORK_DIR="$SCRIPT_DIR/.work"

QUIET=0
ONLY=""

# Java/Kotlin is opt-in because it requires a Maven build and the native FFI dylib on the path.
# Set ASTER_MATRIX_INCLUDE_JAVA=1 to include {java,kotlin} in the server/client lists.
INCLUDE_JAVA="${ASTER_MATRIX_INCLUDE_JAVA:-0}"
JAVA_BINDINGS_DIR="$REPO_ROOT/bindings/java"
JAVA_FFI_LIB="${IROH_LIB_PATH:-$REPO_ROOT/target/release/libaster_transport_ffi.dylib}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    -q|--quiet) QUIET=1; shift ;;
    --only) ONLY="$2"; shift 2 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# *//'
      exit 0
      ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
done

# ── Output helpers ──────────────────────────────────────────────────────────

C_RED='\033[31m'
C_GREEN='\033[32m'
C_BOLD='\033[1m'
C_DIM='\033[2m'
C_RESET='\033[0m'

PASS_TOTAL=0
FAIL_TOTAL=0
declare -a RESULTS=()

run_combo() {
  local label="$1"
  local rc="$2"
  local pass="$3"
  local fail="$4"
  PASS_TOTAL=$((PASS_TOTAL + pass))
  FAIL_TOTAL=$((FAIL_TOTAL + fail))
  if [[ $rc -eq 0 && $fail -eq 0 ]]; then
    RESULTS+=("${C_GREEN}✓${C_RESET} $label  ($pass passed)")
  else
    RESULTS+=("${C_RED}✗${C_RESET} $label  ($pass passed, $fail failed)")
  fi
}

# ── Server lifecycle ────────────────────────────────────────────────────────

start_python_server() {
  local mode="$1"
  local pid_file="$WORK_DIR/server.pid"
  local addr_file="$WORK_DIR/server.addr"
  local log_file="$WORK_DIR/server.log"

  rm -f "$pid_file" "$addr_file" "$log_file"

  local env_prefix=""
  if [[ "$mode" == "auth" ]]; then
    env_prefix="ASTER_ROOT_PUBKEY_FILE=$WORK_DIR/root.pub ASTER_ALLOW_ALL_CONSUMERS=false"
  fi

  cd "$REPO_ROOT"
  if [[ "$mode" == "auth" ]]; then
    eval "$env_prefix uv run python -c \"
import asyncio, os
from aster import AsterServer, AsterConfig
from examples.python.mission_control.services_auth import MissionControl, AgentSession

async def main():
    config = AsterConfig.from_env()
    config.allow_all_consumers = False
    srv = AsterServer(services=[MissionControl(), AgentSession()], config=config)
    await srv.start()
    print(srv.address, flush=True)
    await srv.serve()

asyncio.run(main())
\" > '$addr_file' 2> '$log_file' &"
  else
    uv run python -c "
import asyncio
from aster import AsterServer
from examples.python.mission_control.services import MissionControl, AgentSession

async def main():
    srv = AsterServer(services=[MissionControl(), AgentSession()])
    await srv.start()
    print(srv.address, flush=True)
    await srv.serve()

asyncio.run(main())
" > "$addr_file" 2> "$log_file" &
  fi

  echo $! > "$pid_file"

  # Wait for address to appear (up to 15s)
  local i=0
  while [[ $i -lt 30 ]]; do
    if [[ -s "$addr_file" ]] && grep -q '^aster1' "$addr_file"; then
      return 0
    fi
    sleep 0.5
    i=$((i + 1))
  done

  echo "ERROR: python server did not start within 15s. Log:" >&2
  cat "$log_file" >&2
  return 1
}

start_ts_server() {
  local mode="$1"
  local pid_file="$WORK_DIR/server.pid"
  local addr_file="$WORK_DIR/server.addr"
  local log_file="$WORK_DIR/server.log"

  rm -f "$pid_file" "$addr_file" "$log_file"

  cd "$REPO_ROOT/bindings/typescript"

  if [[ "$mode" == "auth" ]]; then
    ASTER_ROOT_PUBKEY_FILE="$WORK_DIR/root.pub" \
      bun run "$SCRIPT_DIR/_ts_server_auth.ts" > "$addr_file" 2> "$log_file" &
  else
    bun run "$SCRIPT_DIR/_ts_server_dev.ts" > "$addr_file" 2> "$log_file" &
  fi

  echo $! > "$pid_file"

  local i=0
  while [[ $i -lt 30 ]]; do
    if [[ -s "$addr_file" ]] && grep -q 'aster1' "$addr_file"; then
      return 0
    fi
    sleep 0.5
    i=$((i + 1))
  done

  echo "ERROR: ts server did not start within 15s. Log:" >&2
  cat "$log_file" >&2
  return 1
}

start_java_server() {
  local mode="$1"
  local pid_file="$WORK_DIR/server.pid"
  local addr_file="$WORK_DIR/server.addr"
  local log_file="$WORK_DIR/server.log"

  rm -f "$pid_file" "$addr_file" "$log_file"

  # mvn exec:java re-uses installed classes, but we still invoke in-module so no
  # `install` step is required. Spotbugs/fmt/checkstyle are skipped for speed.
  cd "$JAVA_BINDINGS_DIR"
  local main_class exec_args
  if [[ "$mode" == "auth" ]]; then
    main_class=site.aster.examples.missioncontrol.ServerAuth
    exec_args="--strict"
  else
    main_class=site.aster.examples.missioncontrol.Server
    exec_args=""
  fi

  IROH_LIB_PATH="$JAVA_FFI_LIB" \
    mvn -pl aster-examples-mission-control exec:java \
      -Dexec.mainClass="$main_class" \
      -Dexec.args="$exec_args" \
      -Dspotbugs.skip=true -Dfmt.skip=true -Dcheckstyle.skip=true -q \
    > "$addr_file" 2> "$log_file" &

  echo $! > "$pid_file"

  local i=0
  while [[ $i -lt 60 ]]; do
    if [[ -s "$addr_file" ]] && grep -q '^aster1' "$addr_file"; then
      return 0
    fi
    sleep 0.5
    i=$((i + 1))
  done

  echo "ERROR: java server did not start within 30s. Log:" >&2
  cat "$log_file" >&2
  return 1
}

stop_server() {
  local pid_file="$WORK_DIR/server.pid"
  if [[ -f "$pid_file" ]]; then
    local pid
    pid=$(cat "$pid_file")
    kill "$pid" 2>/dev/null || true
    # mvn exec:java spawns child JVMs; kill the whole process group too.
    kill -- "-$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    # Any leftover mvn/java processes spawned by the Java server path.
    pkill -f "site.aster.examples.missioncontrol.Server" 2>/dev/null || true
    pkill -f "site.aster.examples.missioncontrol.ServerAuth" 2>/dev/null || true
    rm -f "$pid_file"
  fi
}

extract_addr() {
  grep -o 'aster1[A-Za-z0-9]*' "$WORK_DIR/server.addr" | head -1
}

# ── Combo runners ───────────────────────────────────────────────────────────

run_python_client() {
  local address="$1"
  local mode="$2"
  local extra=""
  [[ "$mode" == "auth" ]] && extra="--keys-dir $WORK_DIR"
  [[ "$mode" == "dev" ]] && extra="--work-dir $WORK_DIR"
  local q_flag=""
  [[ $QUIET -eq 1 ]] && q_flag="-q"

  cd "$REPO_ROOT"
  uv run python "$SCRIPT_DIR/test_guide.py" "$address" --mode "$mode" $extra $q_flag
}

run_ts_client() {
  local address="$1"
  local mode="$2"
  local extra=""
  [[ "$mode" == "auth" ]] && extra="--keys-dir $WORK_DIR"
  local q_flag=""
  [[ $QUIET -eq 1 ]] && q_flag="-q"

  cd "$REPO_ROOT/bindings/typescript"
  bun run "$SCRIPT_DIR/test_guide.ts" "$address" --mode "$mode" $extra $q_flag
}

run_kotlin_client() {
  local address="$1"
  local mode="$2"
  local extra_args=""
  [[ "$mode" == "auth" ]] && extra_args="--keys-dir $WORK_DIR"
  local q_flag=""
  [[ $QUIET -eq 1 ]] && q_flag="-q"

  cd "$JAVA_BINDINGS_DIR"
  IROH_LIB_PATH="$JAVA_FFI_LIB" \
    mvn -pl aster-examples-mission-control-guide exec:java \
      -Dexec.mainClass=site.aster.examples.missioncontrol.guide.TestGuideKt \
      -Dexec.args="$address --mode $mode $extra_args $q_flag" \
      -Dspotbugs.skip=true -Dfmt.skip=true -Dcheckstyle.skip=true -q
}

run_combo_with_server() {
  local server_lang="$1"
  local client_lang="$2"
  local mode="$3"
  local label="$server_lang-server + $client_lang-client ($mode)"

  if [[ -n "$ONLY" ]]; then
    local combo="${server_lang:0:2}-${client_lang:0:2}-${mode}"
    [[ "$combo" != "$ONLY" ]] && return 0
  fi

  [[ $QUIET -eq 0 ]] && echo -e "\n${C_BOLD}━━━ $label ━━━${C_RESET}"

  # Start the right server
  case "$server_lang" in
    python)
      if ! start_python_server "$mode"; then
        run_combo "$label" 1 0 1
        return
      fi
      ;;
    ts)
      if ! start_ts_server "$mode"; then
        run_combo "$label" 1 0 1
        return
      fi
      ;;
    java)
      if ! start_java_server "$mode"; then
        run_combo "$label" 1 0 1
        return
      fi
      ;;
    *)
      echo "ERROR: unknown server lang: $server_lang" >&2
      run_combo "$label" 1 0 1
      return
      ;;
  esac

  local address
  address=$(extract_addr)
  if [[ -z "$address" ]]; then
    [[ $QUIET -eq 0 ]] && echo "ERROR: could not extract address from $WORK_DIR/server.addr"
    stop_server
    run_combo "$label" 1 0 1
    return
  fi

  # Run the client
  local client_output
  case "$client_lang" in
    python) client_output=$(run_python_client "$address" "$mode" 2>&1) ;;
    ts) client_output=$(run_ts_client "$address" "$mode" 2>&1) ;;
    kotlin) client_output=$(run_kotlin_client "$address" "$mode" 2>&1) ;;
    *)
      echo "ERROR: unknown client lang: $client_lang" >&2
      stop_server
      run_combo "$label" 1 0 1
      return
      ;;
  esac
  local client_rc=$?

  # Print the client output
  echo "$client_output"

  # Parse pass/fail counts from the "Result: X passed, Y failed" line (or the quiet
  # "kt-client dev: X pass, Y fail" form). Tail window is generous because the Kotlin
  # client's JVM-shutdown Cleaner messages tail the stream on stderr.
  local pass fail
  pass=$(echo "$client_output" | tail -30 | grep -oE '[0-9]+ pass' | grep -oE '[0-9]+' | tail -1)
  fail=$(echo "$client_output" | tail -30 | grep -oE '[0-9]+ fail' | grep -oE '[0-9]+' | tail -1)
  [[ -z "$pass" ]] && pass=0
  [[ -z "$fail" ]] && fail=0

  stop_server
  run_combo "$label" "$client_rc" "$pass" "$fail"
}

# ── Main ────────────────────────────────────────────────────────────────────

mkdir -p "$WORK_DIR"

# Set up auth credentials once (used by all auth combos)
if [[ ! -f "$WORK_DIR/root.key" ]] || [[ ! -f "$WORK_DIR/edge.cred" ]] || [[ ! -f "$WORK_DIR/ops.cred" ]]; then
  [[ $QUIET -eq 0 ]] && echo -e "${C_DIM}Setting up auth credentials...${C_RESET}"
  bash "$SCRIPT_DIR/setup_auth.sh" "$WORK_DIR" >/dev/null
fi

# Trap to ensure server cleanup on exit/interrupt
trap 'stop_server' EXIT INT TERM

# Assemble the server / client lists based on the include flag.
SERVERS=(python ts)
CLIENTS=(python ts)
if [[ "$INCLUDE_JAVA" == "1" ]]; then
  SERVERS+=(java)
  CLIENTS+=(kotlin)
  [[ $QUIET -eq 0 ]] && echo -e "${C_DIM}Java server + Kotlin client included (ASTER_MATRIX_INCLUDE_JAVA=1)${C_RESET}"
fi

# Run the matrix
for server in "${SERVERS[@]}"; do
  for client in "${CLIENTS[@]}"; do
    for mode in dev auth; do
      run_combo_with_server "$server" "$client" "$mode"
    done
  done
done

# Final summary
echo
echo -e "${C_BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${C_RESET}"
echo -e "${C_BOLD}Mission Control Matrix Results${C_RESET}"
echo -e "${C_BOLD}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${C_RESET}"
for r in "${RESULTS[@]}"; do
  echo -e "  $r"
done
echo
echo -e "  Total: ${C_GREEN}${PASS_TOTAL} passed${C_RESET}, ${C_RED}${FAIL_TOTAL} failed${C_RESET}"
echo

if [[ $FAIL_TOTAL -eq 0 ]]; then
  [[ $QUIET -eq 0 ]] && echo -e "${C_GREEN}All tests passed — cleaning up .work directory${C_RESET}"
  rm -rf "$WORK_DIR"
  exit 0
else
  echo -e "${C_RED}Failures detected — .work directory preserved at $WORK_DIR${C_RESET}"
  echo -e "${C_DIM}  server.log, generated/, credentials retained for inspection${C_RESET}"
  exit 1
fi
