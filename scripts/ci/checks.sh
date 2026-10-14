#!/usr/bin/env bash
# checks.sh -- the fast CI checks, by name, shared by the `lint` job in
# .forgejo/workflows/linux.yml and scripts/ci/preflight.sh, so a preflight
# runs exactly what CI runs.
#
#   scripts/ci/checks.sh <check>...   run the named checks in order
#   scripts/ci/checks.sh lint         every check the lint job runs
#   scripts/ci/checks.sh preflight    lint plus python-collect
#   scripts/ci/checks.sh --list       print the check names
#
# Run from the repository root. Each check stops on its first failure.
set -euo pipefail

cd "$(dirname "$0")/../.."

LINT_CHECKS=(
  cli-compat guard-tests fmt clippy-workspace coordination-deps otel-deps
  deny clippy-coordination clippy-binding-support clippy-ffi-fixture
  clippy-napi clippy-enrollment clippy-exec publish-manifests
)

check() {
  case "$1" in
    cli-compat)
      python3 scripts/check-cli-compat.py ;;
    guard-tests)
      python3 -S -m unittest discover -s scripts/ci -p 'test_*.py' ;;
    fmt)
      cargo fmt --all -- --check ;;
    clippy-workspace)
      cargo clippy --locked --workspace --all-targets --exclude aster-transport-napi -- -D warnings ;;
    # Feature-graph gates: OpenRaft out of the default and coordination-client
    # graphs; the OpenTelemetry SDK out of aster's default, rpc-only and
    # otel-less binding graphs. Metadata only, no compilation.
    coordination-deps)
      scripts/check-coordination-deps.sh ;;
    otel-deps)
      scripts/check-otel-deps.sh ;;
    # Advisories (every vulnerability must be fixed; deny.toml lists only
    # unmaintained crates with no maintained parent, each with a reason),
    # licences and sources.
    deny)
      if [[ "$(cargo deny --version 2>/dev/null)" != "cargo-deny 0.19.0" ]]; then
        RUSTC_WRAPPER= cargo install --locked --version 0.19.0 cargo-deny
      fi
      cargo deny --locked check ;;
    # The workspace lint builds default features, which compile no
    # coordination code at all; both graphs are linted separately.
    clippy-coordination)
      cargo clippy --locked -p aster --lib --all-targets \
        --no-default-features --features coordination -- -D warnings
      cargo clippy --locked -p aster --lib --all-targets \
        --no-default-features --features coordination-client -- -D warnings ;;
    # `binding-support` is off by default (BINDINGS_CATCHUP_PLAN.md R1; `otel`
    # and `health` since observability O2).
    clippy-binding-support)
      cargo clippy --locked -p aster --all-targets \
        --features rpc,binding-support,otel,health -- -D warnings ;;
    # `tests/coordination.rs` (C ABI 4.4) needs `coordination-fixture`.
    clippy-ffi-fixture)
      cargo clippy --locked -p aster_transport_ffi --all-targets \
        --features coordination-fixture,expose-fixture -- -D warnings ;;
    # The NAPI [lib] is cdylib-only and `--all-targets` mis-flags `#[napi]`
    # exports as dead code, so lint the library target alone.
    clippy-napi)
      cargo clippy --locked -p aster-transport-napi -- -D warnings ;;
    # The enrollment-only graphs (no rpc) and their opt-in test suites.
    clippy-enrollment)
      cargo clippy --locked -p aster_transport_core --features enrollment --lib \
        --test enrollment --test enrollment_issuance --test enrollment_records \
        --test enrollment_review_regressions -- -D warnings
      cargo clippy --locked -p aster --no-default-features --features enrollment,docs \
        --test enrollment --test enrollment_bootstrap --test enrollment_review_regressions -- -D warnings ;;
    clippy-exec)
      cargo clippy --locked -p aster-exec --features service --all-targets \
        -p aster-exec-client -p aster-cli -- -D warnings ;;
    publish-manifests)
      scripts/release/publish-cargo.sh --check ;;
    # Preflight only: the test job's pytest run, collection only, over a debug
    # build of the native module (CI tests a release wheel).
    python-collect)
      local venv="${ASTER_CHECKS_VENV:-.venv}"
      uv venv --allow-existing "$venv"
      uv pip install --python "$venv/bin/python" \
        pytest pytest-asyncio pytest-timeout maturin \
        "pyfory==1.7.5" -r bindings/python/requirements-test.txt
      VIRTUAL_ENV="$venv" "$venv/bin/maturin" develop --locked --uv \
        -m bindings/python/rust/Cargo.toml
      uv pip install --python "$venv/bin/python" -e cli/
      "$venv/bin/pytest" --collect-only -q -o reruns=0 tests/python/ tests/chaos/ \
        -m "not network and not iroh_primitives" ;;
    *)
      echo "checks.sh: unknown check '$1' (see --list)" >&2
      return 2 ;;
  esac
}

[[ $# -gt 0 ]] || { sed -n '2,12p' "$0"; exit 2; }

names=()
for arg in "$@"; do
  case "$arg" in
    --list) printf '%s\n' "${LINT_CHECKS[@]}" python-collect; exit 0 ;;
    lint) names+=("${LINT_CHECKS[@]}") ;;
    preflight) names+=("${LINT_CHECKS[@]}" python-collect) ;;
    *) names+=("$arg") ;;
  esac
done

for name in "${names[@]}"; do
  echo "::group::check $name"
  start=$SECONDS
  check "$name"
  echo "::endgroup::"
  echo "check $name passed ($((SECONDS - start))s)"
done
