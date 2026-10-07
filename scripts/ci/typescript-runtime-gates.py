#!/usr/bin/env python3
"""Run each existing Node/Bun gate once and retain both outcomes on failure."""
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]


def gates(root, temporary):
    package = root / "bindings/typescript/packages/aster"
    report = temporary / "o7-vitest.json"
    return [
        ("node", "vitest", package, ["npx", "vitest", "run", "--reporter=default", "--reporter=json", f"--outputFile={report}"]),
        ("node", "observability-counts", package, ["node", "../../scripts/check-vitest-count.mjs", str(report), "observability-logs.test.ts=7", "observability-export.test.ts=12", "o7-server-health.test.ts=2"]),
        ("node", "smoke", root, ["node", "tests/typescript/smoke/o7-observability-smoke.mjs"]),
        ("bun", "smoke", root, ["bun", "tests/typescript/smoke/o7-observability-smoke.mjs"]),
        ("bun", "package-load", package, ["bun", "--eval", "const m = await import('./dist/index.js'); for (const k of ['AsterServer', 'encodeStreamHeader']) { if (!(k in m)) throw new Error('missing export ' + k) } console.log('ok')"]),
    ]


def run(root, output, temporary, invoke=subprocess.run):
    # A fresh file prevents a failed O7 invocation consuming stale counts.
    (temporary / "o7-vitest.json").unlink(missing_ok=True)
    outcomes = {"schema": 1, "sha": os.environ.get("FORGEJO_SHA"), "run_id": os.environ.get("FORGEJO_RUN_ID"), "attempt": os.environ.get("FORGEJO_RUN_ATTEMPT"), "runtimes": {"node": [], "bun": []}}
    env = {**os.environ, "ASTER_LOG_STDERR": "1", "ASTER_LOG": "warn,aster::lease::diagnostic=debug"}
    failed = False
    for runtime, name, cwd, argv in gates(root, temporary):
        print(f"::group::{runtime}: {name}", flush=True)
        started = time.monotonic()
        try:
            code = invoke(argv, cwd=cwd, env=env, check=False).returncode
        except OSError as error:
            print(f"::error::{runtime} {name}: {error}", flush=True)
            code = 127
        outcomes["runtimes"][runtime].append({"gate": name, "exit_code": code, "seconds": round(time.monotonic() - started, 3)})
        failed |= code != 0
        print("::endgroup::", flush=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(outcomes, indent=2) + "\n")
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(run(ROOT, ROOT / "runtime-evidence/typescript.json", Path(os.environ["RUNNER_TEMP"])))
