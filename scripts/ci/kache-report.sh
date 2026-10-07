#!/usr/bin/env bash
# Compiler events use the parent of Cargo's target; Linux shares that root.
set -euo pipefail
target="${CARGO_TARGET_DIR:?CARGO_TARGET_DIR is required}"
compiler_root="$(dirname "$target")"
mkdir -p cache-evidence
if [[ "$compiler_root" == /cache/kache/targets ]]; then
  scope=shared-linux-aggregate
  echo 'Kache parent-root statistics are shared across concurrent Linux jobs, not job-attributed.'
else
  scope=current-job-parent
fi
python3 - "$compiler_root" "$target" "$scope" <<'PY'
import json, os, sys
from pathlib import Path
Path('cache-evidence/scope.json').write_text(json.dumps({
    'compiler_root': sys.argv[1], 'cargo_target': sys.argv[2], 'scope': sys.argv[3],
    'sha': os.environ.get('FORGEJO_SHA'), 'job': os.environ.get('FORGEJO_JOB'),
    'run_id': os.environ.get('FORGEJO_RUN_ID'), 'window': '3h'
}, indent=2) + '\n')
PY
"$RUSTC_WRAPPER" report --since 3h --root "$compiler_root" --top 15
"$RUSTC_WRAPPER" report --since 3h --root "$compiler_root" --format json --output cache-evidence/compiler-root.json
if [[ "$scope" == shared-linux-aggregate ]]; then
  # Narrow evidence is useful, but omits the majority of shared-root events.
  "$RUSTC_WRAPPER" report --since 3h --root "$target" --format json --output cache-evidence/cargo-target.json
fi
